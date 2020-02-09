use std::cell::Cell;
use std::fmt;
use std::sync::mpsc::{channel, Receiver};
use std::time::Duration;

use futures::Future;
use rayon::{ThreadPool, ThreadPoolBuilder};
use uuid::Uuid;

use labrpc::Error;

use crate::proto::kvraftpb::*;
use crate::select_idx;

#[derive(Debug)]
enum Op {
    Put(String, String),
    Append(String, String),
}

pub struct Clerk {
    pub name: String,
    pub servers: Vec<KvClient>,
    // You will have to modify this struct.
    leader: Cell<Option<usize>>,
    rpc_execution_poll: ThreadPool,
}

impl Op {
    fn into_request(self, client: String) -> PutAppendRequest {
        match self {
            Op::Put(key, value) => PutAppendRequest {
                id: Clerk::new_id(),
                key,
                value,
                op: 1,
                client,
            },
            Op::Append(key, value) => PutAppendRequest {
                id: Clerk::new_id(),
                key,
                value,
                op: 2,
                client,
            },
        }
    }
}

impl fmt::Debug for Clerk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Clerk").field("name", &self.name).finish()
    }
}

impl Clerk {
    pub fn new(name: String, servers: Vec<KvClient>) -> Clerk {
        // You'll have to add code here.
        // Clerk { name, servers }
        let pool = ThreadPoolBuilder::new()
            .num_threads(servers.len() * 2)
            .build()
            .unwrap();
        Clerk {
            name,
            servers,
            leader: Cell::new(None),
            rpc_execution_poll: pool,
        }
    }

    fn run_async<I, E>(
        &self,
        f: impl Future<Item = I, Error = E> + Send + 'static,
    ) -> Receiver<Result<I, E>>
    where
        I: Send + 'static,
        E: Send + 'static,
    {
        let (sx, rx) = channel();
        self.rpc_execution_poll.spawn(move || {
            let _ = sx.send(f.wait());
        });
        rx
    }

    fn new_id() -> Vec<u8> {
        let id = Uuid::new_v4();
        id.as_bytes().to_vec()
    }

    fn try_send_to_current_leader<R>(
        &self,
        send: impl Fn(&KvClient) -> Receiver<R>,
        is_leader: impl Fn(&R) -> bool,
        timeout: Duration,
    ) -> Option<R> {
        if let Some(leader) = self.leader.get() {
            debug!("{}: we have leader {}, sending~", self.name, leader);
            let message = send(&self.servers[leader]).recv_timeout(timeout);
            if message.is_err() {
                debug!("{}: leader {} is timeout :(", self.name, leader);
                self.leader.set(None);
                return None;
            }

            let message = message.unwrap();
            return if !is_leader(&message) {
                // leadership changed.
                debug!("{}: leader {} is died :(", self.name, leader);
                self.leader.set(None);
                None
            } else {
                Some(message)
            };
        }

        None
    }

    fn check_leader_and_send<R: Send + 'static>(
        &self,
        send: impl Fn(&KvClient) -> Receiver<R>,
        is_leader: impl Fn(&R) -> bool,
        timeout: Duration,
    ) -> R {
        debug!("{}: No leader found, but we are seeking ;)", self.name);
        loop {
            let sent = self.servers.iter().map(|client| send(client));
            let send_items = select_idx(sent);
            use std::sync::mpsc::RecvTimeoutError;
            loop {
                match send_items.recv_timeout(timeout) {
                    Ok((i, result)) => {
                        if is_leader(&result) {
                            debug!("We found leader {}!", i);
                            self.leader.set(Some(i));
                            return result;
                        }
                    }
                    Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => break,
                }
            }
            // ensure that there is a leader elected.
            info!("CHECK_LEADER: failed to find leader... retrying...");
            std::thread::sleep(Duration::from_millis(300));
        }
    }

    fn request<R: Send + 'static>(
        &self,
        send: impl Fn(&KvClient) -> Receiver<R>,
        is_leader: impl Fn(&R) -> bool,
        timeout: Duration,
    ) -> R {
        // first: send to current leader.
        let try_result = self.try_send_to_current_leader(&send, &is_leader, timeout);
        if let Some(message) = try_result {
            return message;
        }

        // when leadership changed... or we cannot detect that who is the leader...
        assert!(
            self.leader.get().is_none(),
            "We tried to find new leader even there is a leader available."
        );
        self.check_leader_and_send(&send, &is_leader, timeout)
    }

    /// fetch the current value for a key.
    /// returns "" if the key does not exist.
    /// keeps trying forever in the face of all other errors.
    //
    // you can send an RPC with code like this:
    // if let Some(reply) = self.servers[i].get(args).wait() { /* do something */ }
    pub fn get(&self, key: String) -> String {
        info!("{}: get({:?})", self.name, key);
        let args = GetRequest {
            id: Self::new_id(),
            key: key.clone(),
            client: self.name.clone(),
        };

        let send = |client: &KvClient| self.run_async(client.get(&args));
        let is_leader = |reply: &Result<GetReply, Error>| match reply {
            Err(_) => false,
            Ok(message) if message.wrong_leader => false,
            _ => true,
        };

        loop {
            match self.request(&send, &is_leader, Duration::from_millis(300)) {
                Ok(ref message) if message.err.is_empty() => {
                    info!("get({:?}) => {:?}", key, message);
                    return message.value.clone();
                }
                Ok(ref message) => info!("GET: occurs error: {}, retrying...", message.err),
                Err(_) => {
                    info!("GET: failed to send request");
                    std::thread::sleep(Duration::from_millis(200))
                }
            }
        }
    }

    /// shared by Put and Append.
    //
    // you can send an RPC with code like this:
    // let reply = self.servers[i].put_append(args).unwrap();
    fn put_append(&self, op: Op) {
        info!("{}: put_append({:?})", self.name, op);
        // You will have to modify this function.
        let args: PutAppendRequest = op.into_request(self.name.clone());
        let send = |client: &KvClient| self.run_async(client.put_append(&args));
        let is_leader = |reply: &Result<PutAppendReply, Error>| match reply {
            Err(_) => false,
            Ok(message) if message.wrong_leader => false,
            _ => true,
        };

        loop {
            match self.request(&send, &is_leader, Duration::from_millis(300)) {
                Ok(ref result) if result.err.is_empty() => {
                    info!("put_append({:?}) => {:?}", args, result);
                    return;
                }
                _ => {
                    info!(
                        "{}: put_append({:?}) failed, sleeping before resend...",
                        self.name, args
                    );
                    std::thread::sleep(Duration::from_millis(200))
                }
            }
        }
    }

    pub fn put(&self, key: String, value: String) {
        self.put_append(Op::Put(key, value))
    }

    pub fn append(&self, key: String, value: String) {
        self.put_append(Op::Append(key, value))
    }
}
