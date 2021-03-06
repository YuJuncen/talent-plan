use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use failure::Fail;
use futures::sync::mpsc::{unbounded, UnboundedReceiver};
use futures::sync::oneshot::Sender;
use futures::{Future, Sink, Stream};
use futures_timer::Delay;
use uuid::Uuid;

use labcodec::{decode, encode, Message};
use labrpc::RpcFuture;

use crate::async_rpc;
use crate::kvraft::server::KvError::{FailToCommit, Timeout};
use crate::proto::kvraftpb::*;
use crate::raft;
use crate::raft::{ApplyMsg, SnapshotFile};

/// The generic type of boxed future.
type FutureRef<R, E> = Box<dyn Future<Item = R, Error = E> + Send + 'static>;

#[derive(Clone, Debug)]
enum KvCommand {
    Get {
        id: Uuid,
        key: String,
        client: String,
    },
    Put {
        id: Uuid,
        key: String,
        value: String,
        client: String,
    },
    Append {
        id: Uuid,
        key: String,
        value: String,
        client: String,
    },
}

fn build_uuid(origin: &[u8]) -> Uuid {
    Uuid::from_slice(origin).expect("Failed to parse uuid from request.")
}

impl KvCommand {
    fn from_bytes(proto: &[u8]) -> Option<Self> {
        if let Ok(putting) = labcodec::decode::<PutAppendRequest>(proto) {
            return Some(KvCommand::from_put_append(putting));
        }
        if let Ok(getting) = labcodec::decode::<GetRequest>(proto) {
            return Some(KvCommand::from_get(getting));
        }
        warn!("failed to parse proto buffer message {:?}.", proto);
        None
    }

    fn from_put_append(request: PutAppendRequest) -> Self {
        match Op::from_i32(request.op).unwrap_or(Op::Unknown) {
            Op::Unknown => panic!("unknown op detached: {:?}.", request),
            Op::Put => KvCommand::Put {
                id: build_uuid(request.id.as_slice()),
                key: request.key,
                value: request.value,
                client: request.client,
            },
            Op::Append => KvCommand::Append {
                id: build_uuid(request.id.as_slice()),
                key: request.key,
                value: request.value,
                client: request.client,
            },
        }
    }

    fn from_get(request: GetRequest) -> Self {
        KvCommand::Get {
            id: build_uuid(request.id.as_slice()),
            key: request.key,
            client: request.client,
        }
    }

    fn get_id(&self) -> Uuid {
        *match self {
            KvCommand::Get { id, .. } => id,
            KvCommand::Put { id, .. } => id,
            KvCommand::Append { id, .. } => id,
        }
    }

    fn get_client(&self) -> &str {
        match self {
            KvCommand::Get { client, .. } => client,
            KvCommand::Put { client, .. } => client,
            KvCommand::Append { client, .. } => client,
        }
        .as_str()
    }
}

#[derive(Fail, Debug)]
enum KvError {
    #[fail(display = "Raft internal error.")]
    Raft(raft::errors::Error),
    #[fail(display = "Current node isn't leader.")]
    NotLeader,
    #[fail(display = "The command failed to commit.")]
    FailToCommit,
    #[fail(
        display = "The command spend too mach time for commit, maybe leader is died or network partition occurs."
    )]
    Timeout,
}

pub mod err_codes {
    pub const KVERR_RAFT: u32 = 1;
    pub const KVERR_NOT_LEADER: u32 = 2;
    pub const KVERR_FAIL_TO_COMMIT: u32 = 3;
    pub const KVERR_TIMEOUT: u32 = 4;
    pub const KVERR_CLOESD: u32 = 5;
}

impl KvError {
    fn get_code(&self) -> u32 {
        use self::err_codes::*;
        use KvError::*;
        match self {
            Raft(_) => KVERR_RAFT,
            NotLeader => KVERR_NOT_LEADER,
            FailToCommit => KVERR_FAIL_TO_COMMIT,
            Timeout => KVERR_TIMEOUT,
        }
    }
}

type Result<T> = std::result::Result<T, KvError>;

#[derive(Clone)]
struct KvStateMachine {
    /// Current state.
    state: Arc<Mutex<BTreeMap<String, String>>>,
    /// Last command id of each client.
    last_command: Arc<Mutex<HashMap<String, Uuid>>>,
    /// `Waker` for commands started, but waiting to be committed.
    waiting_channels: Arc<Mutex<BTreeMap<u64, Sender<CommandResponse>>>>,
    /// The consensus algorithm.
    raft: raft::Node,
    /// The state machine name, for debug usage only.
    name: String,
    /// Should the current state machine log.
    should_log: bool,
    /// The internal channel to stop state machine.
    cancel_ch: Arc<futures::sync::mpsc::Sender<Option<ApplyMsg>>>,
    /// Max raft state, if raft log grow too large, will trigger a snapshot.
    ///
    /// `None` for never snapshot.
    max_size: Option<usize>,
    /// Index of last applied command to the state machine.
    last_index: Arc<AtomicUsize>,
}

/// Command basic abstraction.
trait Command {
    /// Get the command id.
    fn get_id(&self) -> Uuid;
    /// Get whether the command is read-only.
    fn is_readonly(&self) -> bool;
}

impl Command for PutAppendRequest {
    fn get_id(&self) -> Uuid {
        Uuid::from_slice(self.id.as_slice()).unwrap()
    }

    fn is_readonly(&self) -> bool {
        false
    }
}

impl Command for GetRequest {
    fn get_id(&self) -> Uuid {
        Uuid::from_slice(self.id.as_slice()).unwrap()
    }

    fn is_readonly(&self) -> bool {
        true
    }
}

impl Command for KvCommand {
    fn get_id(&self) -> Uuid {
        self.get_id()
    }

    fn is_readonly(&self) -> bool {
        match self {
            KvCommand::Get { .. } => true,
            KvCommand::Put { .. } | KvCommand::Append { .. } => false,
        }
    }
}

#[derive(Debug)]
/// a generic response to client of `KvStateMachine`.
struct CommandResponse {
    command_id: Uuid,
    reply: String,
    command_idx: usize,
}

impl CommandResponse {
    fn new(id: Uuid, reply: String, idx: usize) -> Self {
        CommandResponse {
            command_id: id,
            reply,
            command_idx: idx,
        }
    }
}

/// A state machine that records key value.
impl KvStateMachine {
    /// transform state by a committed command.
    fn handle_command(&self, cmd: KvCommand) {
        let id = cmd.get_id();
        let mut history = self.last_command.lock().unwrap();
        if history
            .get(cmd.get_client())
            .map(|i| *i == id)
            .unwrap_or(false)
        {
            if self.should_log {
                warn!(
                    "{}: Replicated ID: {} get, the command {:?} won't be processed.",
                    self.name, id, cmd
                );
            }
            return;
        }
        history.insert(cmd.get_client().to_owned(), id);
        drop(history);

        match cmd {
            KvCommand::Put { key, value, .. } => {
                let mut state: MutexGuard<BTreeMap<String, String>> = self.state.lock().unwrap();
                let map = &mut *state;
                map.insert(key, value);
            }
            KvCommand::Append { key, value, .. } => {
                let mut state = self.state.lock().unwrap();
                state.entry(key).or_default().push_str(value.as_str());
            }
            _ => (),
        }
    }

    /// shutdown the state machine.
    fn shutdown(&self) {
        (&*self.cancel_ch)
            .clone()
            .send(None)
            .wait()
            .unwrap_or_else(|e| {
                panic!(
                    "{} Failed to shutdown kv machine, because: {}",
                    self.name, e
                )
            });
        let mut wc = self.waiting_channels.lock().unwrap();
        // drop all pending channels.
        wc.clear();

        // shrink log size (synchronously) to make tester happy.
        let last_index = self.last_index.load(Ordering::SeqCst);
        self.raft.take_snapshot(self.make_snapshot(), last_index);
    }

    /// notify the waiter that waiting on index `idx`,
    /// with message `msg`.
    fn notify_at(&self, idx: u64, msg: &KvCommand) {
        let mut notifier = self.waiting_channels.lock().unwrap();
        if let Some(sender) = notifier.remove(&idx) {
            let response = if let KvCommand::Get { id, key, .. } = msg {
                let state = self.state.lock().unwrap();
                CommandResponse::new(
                    *id,
                    state.get(key).cloned().unwrap_or_else(|| "".to_owned()),
                    idx as usize,
                )
            } else {
                CommandResponse::new(msg.get_id(), "".to_owned(), idx as usize)
            };
            Sender::send(sender, response).unwrap_or_else(|err| {
                if self.should_log {
                    warn!(
                        "Message notifier doesn't send rightly. \
                         Maybe raft commits {:?} too fast, or too slow.",
                        err
                    );
                }
            });
        }
    }

    /// handle a virtual command from raft snapshot.
    ///
    /// i.e. load data from snapshot.
    fn handle_virtual_command(&self, cmd: &[u8]) {
        let cmd = decode::<VirtualCommand>(cmd).expect("failed to decode virtual command");
        use crate::proto::kvraftpb::virtual_command::Command::*;
        match cmd
            .command
            .expect("handle_virtual_command: cannot parse snapshot file...")
        {
            Ilc(last_commands) => {
                let mut lc = self.last_command.lock().unwrap();
                for (k, v) in last_commands.cmd {
                    lc.insert(k, Uuid::from_slice(&v)
                            .expect("handle_virtual_command: failed to parse uuid from raw bytes from snapshot."));
                }
            }
            Ikv(key_values) => {
                let mut kv = self.state.lock().unwrap();
                for (k, v) in key_values.kvs {
                    kv.insert(k, v);
                }
                self.last_index
                    .store(key_values.last_index as usize, Ordering::SeqCst);
            }
        }
    }

    /// make a snapshot file of current state of the state machine.
    fn make_snapshot(&self) -> SnapshotFile {
        use crate::proto::kvraftpb::virtual_command::Command::*;
        let s = self.state.as_ref().lock().unwrap();
        let state = VirtualCommand {
            command: Some(Ikv(InstallKvs {
                kvs: s.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
                last_index: self.last_index.load(Ordering::SeqCst) as u64,
            })),
        };
        let mut state_u8 = vec![];
        encode(&state, &mut state_u8).unwrap();

        let lc = self.last_command.lock().unwrap();
        let last_command = VirtualCommand {
            command: Some(Ilc(InstallLastCommand {
                cmd: lc
                    .iter()
                    .map(|(k, v)| (k.clone(), v.as_bytes().to_vec()))
                    .collect(),
            })),
        };
        let mut last_command_u8 = vec![];
        encode(&last_command, &mut last_command_u8).unwrap();

        SnapshotFile {
            commands: vec![last_command_u8, state_u8],
        }
    }

    fn new(
        apply_ch: UnboundedReceiver<ApplyMsg>,
        raft: raft::Node,
        name: String,
        should_log: bool,
        max_size: Option<usize>,
    ) -> Self {
        let state = Arc::new(Mutex::new(BTreeMap::new()));
        let success_commands = Arc::new(Mutex::new(HashMap::new()));
        let waiting_channels = Arc::new(Mutex::new(BTreeMap::new()));
        let (do_cancel, cancel) = futures::sync::mpsc::channel(1);
        let fsm = KvStateMachine {
            state,
            last_command: success_commands,
            last_index: Arc::new(AtomicUsize::new(0)),
            waiting_channels,
            raft,
            name,
            should_log,
            cancel_ch: Arc::new(do_cancel),
            max_size,
        };
        std::thread::spawn({
            let fsm = fsm.clone();
            info!("FSM worker for {} start!", fsm.name);
            move || {
                let mut i = apply_ch.map(Some).select(cancel).wait();
                while let Some(Ok(Some(message))) = i.next() {
                    if message.command_valid {
                        let last_index = fsm.last_index.load(Ordering::SeqCst);
                        if last_index >= message.command_index as usize {
                            // this command has been handled.
                            continue;
                        }
                        fsm.handle_message(&message);
                        // don't worry commit_index changes during handle,
                        let commit_idx = fsm.raft.commit_index();
                        if commit_idx > message.command_index {
                            // use try_get_log_between because the operation during this function isn't atomic.
                            let msgs = fsm.raft.try_get_log_between(
                                (message.command_index + 1) as usize,
                                commit_idx as usize,
                            );
                            for msg in msgs {
                                fsm.handle_message(&msg);
                            }
                        }

                        if let Some(max) = fsm.max_size {
                            if fsm.raft.log_size() > (max as f64 * 0.9) as usize {
                                let last_index = fsm.last_index.load(Ordering::SeqCst);
                                fsm.raft.take_snapshot(fsm.make_snapshot(), last_index);
                            }
                        }
                    } else {
                        fsm.handle_virtual_command(message.command.as_slice());
                    }
                }
                info!("FSM worker for {} ends!", fsm.name)
            }
        });
        fsm
    }

    /// handle one message.
    ///
    /// # panics
    /// if the message index not greater than last_index.
    fn handle_message(&self, message: &ApplyMsg) {
        let last_index = self.last_index.load(Ordering::SeqCst);
        let command = KvCommand::from_bytes(message.command.as_slice());
        assert!(
            last_index < message.command_index as usize,
            "handle_message won't handle message that has been handled."
        );
        if command.is_none() {
            panic!("Invalid message received.")
        }
        let cmd = command.unwrap();
        if self.should_log {
            info!(
                "{} {} => idx {} (Committed)",
                self.name,
                cmd.get_id(),
                message.command_index
            );
        }
        self.notify_at(message.command_index, &cmd);
        if !cmd.is_readonly() {
            self.handle_command(cmd);
        }
        self.last_index
            .store(message.command_index as usize, Ordering::SeqCst);
    }

    /// start a new command.
    ///
    /// # returns
    /// The future that presents the result of this command.
    /// If command is `Get`, the value of that key will be returned at `reply` field of `CommandResponse`.
    fn start(&self, cmd: &(impl Message + Command)) -> FutureRef<Result<CommandResponse>, ()> {
        use crate::raft::errors::Error;

        match self.raft.start(cmd) {
            Ok((idx, _term)) => {
                let (sx, rx) = futures::sync::oneshot::channel();
                let mut map = self.waiting_channels.lock().unwrap();
                map.insert(idx, sx);
                let cmd_id = cmd.get_id();
                let should_log = self.should_log;
                info!("{}: cmd {} => idx {}", self.name, cmd.get_id(), idx);
                Box::new(
                    rx.map(move |cmd| {
                        if cmd.command_id == cmd_id {
                            Ok(cmd)
                        } else {
                            Err(FailToCommit)
                        }
                    })
                    .map_err(move |e| {
                        if should_log {
                            error!(
                                "FSM::start received exception: {}, maybe FSM should stop.",
                                e
                            )
                        }
                    }),
                )
            }
            Err(Error::NotLeader) => Box::new(futures::finished(Err(KvError::NotLeader))),
            Err(e) => Box::new(futures::finished(Err(KvError::Raft(e)))),
        }
    }

    /// check some client's operation has done.
    ///
    /// We assume that all clients are SYNCHRONOUS, which means, a client just request once each time.
    /// Before receiving an 'Ok' response from server, it would never start any new request.
    fn has_done(&self, client: &str, id: Uuid) -> bool {
        let commands = self.last_command.lock().unwrap();
        commands.get(client).map(|i| *i == id).unwrap_or(false)
    }
}

/// a thin wrapper of `KvStateMachine`.
/// connect raft and `KvStateMachine`.
pub struct KvServer {
    pub rf: raft::Node,
    #[allow(dead_code)]
    me: usize,
    // snapshot if log grows this big
    maxraftstate: Option<usize>,
    // Your definitions here.
    fsm: KvStateMachine,
}

impl KvServer {
    pub fn new(
        servers: Vec<crate::proto::raftpb::RaftClient>,
        me: usize,
        persister: Box<dyn raft::persister::Persister>,
        maxraftstate: Option<usize>,
    ) -> KvServer {
        // You may need initialization code here.

        let (tx, apply_ch) = unbounded();
        let rf = raft::Raft::new(servers, me, persister, tx);
        let node = raft::Node::new(rf);
        let fsm = KvStateMachine::new(
            apply_ch,
            node.clone(),
            format!("[{}]", me),
            me == 0,
            maxraftstate,
        );
        KvServer {
            rf: node,
            me,
            maxraftstate,
            fsm,
        }
    }
}

impl KvServer {
    /// Only for suppressing deadcode warnings.
    #[doc(hidden)]
    pub fn __suppress_deadcode(&mut self) {
        let _ = &self.maxraftstate;
        crate::your_code_here(());
    }
}

// Choose concurrency paradigm.
//
// You can either drive the kv server by the rpc framework,
//
// ```rust
// struct Node { server: Arc<Mutex<KvServer>> }
// ```
//
// or spawn a new thread runs the kv server and communicate via
// a channel.
//
// ```rust
// struct Node { sender: Sender<Msg> }
// ```
#[derive(Clone)]
pub struct Node {
    server: Arc<Mutex<KvServer>>,
}

static RAFT_COMMIT_TIMEOUT: Duration = Duration::from_millis(300);

fn timeout_fut<T>() -> impl Future<Item = Result<T>, Error = ()> {
    Delay::new(RAFT_COMMIT_TIMEOUT)
        .map(|_| Err(Timeout))
        .map_err(|_| ())
}

impl Node {
    pub fn new(kv: KvServer) -> Node {
        let server = Arc::new(Mutex::new(kv));
        Node { server }
    }

    /// the tester calls Kill() when a KVServer instance won't
    /// be needed again. you are not required to do anything
    /// in Kill(), but it might be convenient to (for example)
    /// turn off debug output from this instance.
    pub fn kill(&self) {
        // Your code here, if desired.
        let server = self.server.lock().unwrap();
        server.rf.kill();
        server.fsm.shutdown();
    }

    /// The current term of this peer.
    pub fn term(&self) -> u64 {
        self.get_state().term()
    }

    /// Whether this peer believes it is the leader.
    pub fn is_leader(&self) -> bool {
        self.get_state().is_leader()
    }

    pub fn get_state(&self) -> raft::State {
        let server = self.server.lock().unwrap();
        server.rf.get_state()
    }

    fn do_get(&self, arg: GetRequest) -> GetReply {
        let server = self.server.lock().unwrap();
        let fsm = server.fsm.clone();
        drop(server);

        let start_result = fsm.start(&arg);
        start_result
            .select(timeout_fut())
            .map(move |(result, _)| match result {
                Err(KvError::NotLeader) => GetReply {
                    wrong_leader: true,
                    err: "not leader".to_owned(),
                    value: "".to_owned(),
                    err_code: KvError::NotLeader.get_code(),
                },
                Ok(cmd) => GetReply {
                    wrong_leader: false,
                    err: "".to_owned(),
                    value: cmd.reply,
                    err_code: 0,
                },
                Err(e) => GetReply {
                    wrong_leader: false,
                    err: format!("ERROR: {}", e),
                    value: "".to_owned(),
                    err_code: e.get_code(),
                },
            })
            .wait()
            .unwrap_or_else(|((), _)| GetReply {
                wrong_leader: false,
                err: "FSM cancels execution.".to_owned(),
                value: "".to_owned(),
                err_code: err_codes::KVERR_CLOESD,
            })
    }

    fn do_put_append(&self, arg: PutAppendRequest) -> PutAppendReply {
        let server = self.server.lock().unwrap();
        let cmd_id = Uuid::from_slice(arg.id.as_slice()).expect("fetal: bad command id.");
        if server.fsm.has_done(arg.client.as_str(), cmd_id) {
            return PutAppendReply {
                wrong_leader: false,
                err: "".to_owned(),
                err_code: 0,
            };
        }
        let start_result = server.fsm.start(&arg);
        drop(server);

        start_result
            .select(timeout_fut())
            .map(|(result, _)| match result {
                Err(KvError::NotLeader) => PutAppendReply {
                    wrong_leader: true,
                    err: "not leader".to_owned(),
                    err_code: KvError::NotLeader.get_code(),
                },
                Ok(_resp) => PutAppendReply {
                    wrong_leader: false,
                    err: "".to_owned(),
                    err_code: 0,
                },
                Err(e) => PutAppendReply {
                    wrong_leader: false,
                    err: format!("ERROR: {}", e),
                    err_code: e.get_code(),
                },
            })
            .wait()
            .unwrap_or_else(|((), _)| PutAppendReply {
                wrong_leader: false,
                err: "FSM cancels execution.".to_owned(),
                err_code: err_codes::KVERR_CLOESD,
            })
    }
}

impl KvService for Node {
    async_rpc! { get(GetRequest) -> GetReply where uses Self::do_get }
    async_rpc! { put_append(PutAppendRequest) -> PutAppendReply where uses Self::do_put_append }
}
