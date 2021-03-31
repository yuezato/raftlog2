use futures::{Async, Future, Poll};
use raftlog::election::{Ballot, Role, Term};
use raftlog::log::{Log, LogIndex, LogPosition, LogPrefix, LogSuffix};
use raftlog::message::Message;
use raftlog::node::NodeId;
use raftlog::{Error, Io, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};

extern crate prometrics;

/*
 * Mock用のIO
 */
pub struct MockIo {
    node_id: NodeId,
    recv: Receiver<Message>,
    send: Sender<Message>, // recvにデータを送るためのsender; コピー可能なので他のノードにコピーして渡す
    channels: BTreeMap<NodeId, Sender<Message>>,
    ballots: Vec<Ballot>,
    snapshotted: Option<LogPrefix>,
    rawlogs: Option<LogSuffix>,
    candidate_invoker: Option<Sender<()>>,
    follower_invoker: Option<Sender<()>>,
    leader_invoker: Option<Sender<()>>,

    ban_list: Vec<String>,
    count: Option<usize>,
}

impl MockIo {
    pub fn snapshot(&self) -> &Option<LogPrefix> {
        &self.snapshotted
    }
    pub fn rawlog(&self) -> &Option<LogSuffix> {
        &self.rawlogs
    }
    
    pub fn set_ban_list(&mut self, list: &[&str]) {
        self.ban_list = list.iter().map(|x| x.to_string()).collect::<Vec<String>>();
    }

    pub fn clear_ban_list(&mut self) {
        self.ban_list = Vec::new();
    }

    pub fn prepare(&mut self) {
        // 2回までは受け取るが
        // 3回目以降は全て処理しない
        self.count = Some(3);
    }
    
    pub fn new(node_id: NodeId) -> Self {
        let (send, recv) = channel();
        Self {
            node_id,
            recv,
            send,
            channels: Default::default(),
            ballots: Vec::new(),
            snapshotted: None,
            rawlogs: None,
            candidate_invoker: None,
            follower_invoker: None,
            leader_invoker: None,
            
            ban_list: Vec::new(),
            count: None,
        }
    }

    pub fn copy_sender(&self) -> Sender<Message> {
        self.send.clone()
    }

    pub fn set_channel(&mut self, node: NodeId, chan: Sender<Message>) {
        self.channels.insert(node, chan);
    }

    pub fn invoke_timer(&self) {
        if let Some(sender) = &self.candidate_invoker {
            sender.send(());
        }

        if let Some(sender) = &self.follower_invoker {
            sender.send(());
        }

        if let Some(sender) = &self.leader_invoker {
            sender.send(());
        }
    }
}

impl Io for MockIo {
    fn try_recv_message(&mut self) -> Result<Option<Message>> {
        let r = self.recv.try_recv();
        if let Ok(v) = r {
            if let Some(c) = self.count {
                self.count = Some(c.saturating_sub(1));
            }
            
            let who = &v.header().sender;
            println!("{:?} <---- {:?}", self.node_id, v.header().sender);

            let who_name: String = who.as_str().to_owned();

            // dbg!(&self.ban_list);
            // dbg!(&who_name);

            if self.count == Some(0) {
                println!("shotout");
                Ok(None)
            } else if self.ban_list.contains(&who_name) {
                // block
                println!("banned!");
                Ok(None)
            } else {
                Ok(Some(v))
            }
        } else {
            let err = r.err().unwrap();
            if err == TryRecvError::Empty {
                Ok(None)
            } else {
                panic!("disconnected");
            }
        }
    }

    fn send_message(&mut self, message: Message) {
        let dest: NodeId = message.header().destination.clone();

        println!("send {:?} ----> {:?}", self.node_id, dest);
        
        if self.ban_list.contains(&dest.as_str().to_string()) {
            println!("^---- BAN!!!");
            return;
        }
        
        let channel = self.channels.get(&dest).unwrap();
        channel.send(message).unwrap();
    }

    type SaveBallot = BallotSaver;
    fn save_ballot(&mut self, ballot: Ballot) -> Self::SaveBallot {
        self.ballots.push(ballot.clone());
        BallotSaver(self.node_id.clone(), ballot)
    }

    // 最後に行った投票状況を取り戻す
    // （それとも、最後にsaveしたものを取り戻す?）
    type LoadBallot = BallotLoader;
    fn load_ballot(&mut self) -> Self::LoadBallot {
        if self.ballots.is_empty() {
            BallotLoader(self.node_id.clone(), None)
        } else {
            let last_pos = self.ballots.len() - 1;
            let ballot = self.ballots[last_pos].clone();
            BallotLoader(self.node_id.clone(), Some(ballot))
        }
    }

    type SaveLog = LogSaver;
    // prefixは必ず前回部分を含んでいるものか？
    // それとも新しい差分か？ 多分前者だと思うが何も分からない
    fn save_log_prefix(&mut self, prefix: LogPrefix) -> Self::SaveLog {
        println!("SAVE SNAPSHOT");
        dbg!(&prefix);
        
        if let Some(snap) = &self.snapshotted {
            // 保存するデータはcommit済みのハズなので
            // この辺は問題ないだろうsnapshotについてはこれが成立するはず
            //
            // と思ったがcommit済みではない
            assert!(prefix.tail.is_newer_or_equal_than(snap.tail));
        }
        self.snapshotted = Some(prefix);
        LogSaver(SaveMode::SnapshotSave, self.node_id.clone())
    }
    fn save_log_suffix(&mut self, suffix: &LogSuffix) -> Self::SaveLog {
        println!("SAVE RAWLOGS");
        dbg!(&suffix);
        
        if let Some(rawlogs) = &mut self.rawlogs {
            // これもcommit済みのデータが来るはずなので通ると思う
            assert!(suffix.head.is_newer_or_equal_than(rawlogs.head));

            // マージする
            rawlogs.merge(suffix);
        } else {
            // 空のdiskへの書き込みに対応する
            
            self.rawlogs = Some(suffix.clone());
        }

        LogSaver(SaveMode::RawLogSave, self.node_id.clone())
    }

    type LoadLog = LogLoader;
    fn load_log(&mut self, start: LogIndex, end: Option<LogIndex>) -> Self::LoadLog {
        dbg!("load log", &start, &end);
        
        if self.snapshotted.is_none() && self.rawlogs.is_none() {
            if start == LogIndex::new(0) && end == None {
                // 無に対するロードなら無を作る
                /*
                let log = LogSuffix {
                    entries: Vec::new(),
                    head: LogPosition {
                        index: LogIndex::new(0),
                        prev_term: Term::new(0xff), // 空に対してのprev_termはどうする???
                    }
                };
                 */
                let log = LogSuffix::default(); // 仕方がないのでdefaultを使う
                return LogLoader(Some(Log::from(log)));
            }
            panic!("Try load from None: start = {:?}, end = {:?}", start, end);
        }
        
        // startから全て読み込みたい場合
        if end == None {
            if let Some(snap) = &self.snapshotted {
                if snap.is_match(start, end) {
                    // snapshotから読み込む必要があるのでまずはsnapshot
                    // https://github.com/frugalos/raftlog/blob/087d8019b42a05dbc5b463db076041d45ad2db7f/src/node_state/loader.rs#L36
                    // ^----この実装により、snapshot読み込みの後で、必要があればrawlogの読み込みが行われる
                    return LogLoader(Some(Log::from(snap.clone())));
                }
            }
            if let Some(rawlogs) = &self.rawlogs {
                let mut rawlogs = rawlogs.clone();
                rawlogs.skip_to(start).unwrap();
                return LogLoader(Some(Log::from(rawlogs)));
            }
            panic!("start = {:?}, end = {:?}", start, end);
        }

        // [start, end) の読み込み
        let end = end.unwrap();

        if start > end {
            dbg!(&self.node_id);
            println!("start = {:?}, end = {:?}", start, end);
            unreachable!(";-|");
        }

        if start == end {
            dbg!(&self.node_id);
            println!("[OK???] start = {:?}, end = {:?}", start, end);
        }
        
        if let Some(snap) = &self.snapshotted {
            if snap.is_match(start, Some(end)) {
                return LogLoader(Some(Log::from(snap.clone())));
            }
        }

        if let Some(rawlogs) = &self.rawlogs {
            if let Ok(sliced) = rawlogs.slice(start, end) {
                return LogLoader(Some(Log::from(sliced)));
            }
        }

        if let Some(snap) = &self.snapshotted {
            snap.dump();
        } else {
            println!("No snapshot");
        }
        println!("rawlog = {:?}", self.rawlogs);
        panic!("start = {:?}, end = {:?}", start, end);
    }

    type Timeout = Invoker;
    fn create_timeout(&mut self, role: Role) -> Self::Timeout {
        self.candidate_invoker = None;
        self.follower_invoker = None;
        self.leader_invoker = None;

        let (invoker, timer) = channel();
        match role {
            Role::Candidate => {
                self.candidate_invoker = Some(invoker);
            }
            Role::Follower => {
                self.follower_invoker = Some(invoker);
            }
            Role::Leader => {
                self.leader_invoker = Some(invoker);
            }
        }
        Invoker(timer)
    }
}

pub struct Invoker(Receiver<()>);

impl Future for Invoker {
    type Item = ();
    type Error = Error;
    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let r = self.0.try_recv();
        if r.is_ok() {
            Ok(Async::Ready(()))
        } else {
            let err = r.err().unwrap();
            if err == TryRecvError::Empty {
                Ok(Async::NotReady)
            } else {
                panic!("disconnected");
            }
        }
    }
}

pub struct LogLoader(Option<Log>);
impl Future for LogLoader {
    type Item = Log;
    type Error = Error;
    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        if let Some(log) = self.0.take() {
            Ok(Async::Ready(log))
        } else {
            panic!("")
        }
    }
}

enum SaveMode {
    SnapshotSave,
    RawLogSave,
}
pub struct LogSaver(SaveMode, pub NodeId);
impl Future for LogSaver {
    type Item = ();
    type Error = Error;
    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        match self.0 {
            SaveMode::SnapshotSave => {
                println!("[Node {}] Save Snapshot", self.1.as_str())
            }
            SaveMode::RawLogSave => {
                println!("[Node {}] Save Raw Logs", self.1.as_str())
            }
        }
        Ok(Async::Ready(()))
    }
}

pub struct BallotSaver(pub NodeId, pub Ballot);
pub struct BallotLoader(pub NodeId, pub Option<Ballot>);

impl Future for BallotSaver {
    type Item = ();
    type Error = Error;
    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        println!(
            "[Node {}] Save Ballot({})",
            self.0.as_str(),
            self.1.to_str()
        );
        Ok(Async::Ready(()))
    }
}

impl Future for BallotLoader {
    type Item = Option<Ballot>;
    type Error = Error;
    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        println!(
            "[Node {}] Load Ballot({:?})",
            self.0.as_str(),
            self.1
        );
        Ok(Async::Ready(self.1.clone()))
    }
}

use raftlog::ReplicatedLog;
use prometrics::metrics::MetricBuilder;
use futures::Stream;
use std::{thread, time};

fn scenario1() {
    println!("Hello, Raft World!");

    let mut cluster = BTreeSet::new();
    let node1 = NodeId::new("nodeA");
    let node2 = NodeId::new("nodeB");
    cluster.insert(node1.clone());
    cluster.insert(node2.clone());
    let mut io1 = MockIo::new(node1.clone());
    let mut io2 = MockIo::new(node2.clone());
    
    let sender1 = io1.copy_sender();
    let sender2 = io2.copy_sender();

    {
        io1.set_channel(node2.clone(), sender2);
        io2.set_channel(node1.clone(), sender1);
    }
    
    let mut rlog1 = ReplicatedLog::new(node1, cluster.clone(), io1, &MetricBuilder::new()).unwrap();
    let mut rlog2 = ReplicatedLog::new(node2, cluster, io2, &MetricBuilder::new()).unwrap();

    use std::io;
    let mut input = String::new();

    loop {
        input.clear();
        io::stdin().read_line(&mut input).unwrap();

        dbg!(&input);
        
        if input == "a\n" {
            unsafe { rlog1.io_mut().invoke_timer(); }
        } else if input == "b\n" {
            unsafe { rlog2.io_mut().invoke_timer(); }
        }

        for _ in 0..50 {
            if let Async::Ready(event) = rlog1.poll().unwrap() {
                println!("[[A]] event {:?}", event);
                // dbg!(rlog1.local_history());
            }

            if let Async::Ready(event) = rlog2.poll().unwrap() {
                println!("[[B]] event {:?}", event);
                // dbg!(rlog2.local_history());
            }
        
            // thread::sleep(time::Duration::from_millis(5));
        }
    }
    
    println!("Bye!");
}

fn scenario2() {
    use std::io::Write;
    
    println!("[Start] Senario2");

    let mut cluster = BTreeSet::new();
    let node1 = NodeId::new("nodeA");
    let node2 = NodeId::new("nodeB");
    let node3 = NodeId::new("nodeC");
    
    cluster.insert(node1.clone());
    cluster.insert(node2.clone());
    cluster.insert(node3.clone());
    let mut io1 = MockIo::new(node1.clone());
    let mut io2 = MockIo::new(node2.clone());
    let mut io3 = MockIo::new(node3.clone());
    
    let sender1 = io1.copy_sender();
    let sender2 = io2.copy_sender();
    let sender3 = io3.copy_sender();
    
    {
        io1.set_channel(node2.clone(), sender2.clone());
        io1.set_channel(node3.clone(), sender3.clone());
        
        io2.set_channel(node1.clone(), sender1.clone());
        io2.set_channel(node3.clone(), sender3.clone());

        io3.set_channel(node1.clone(), sender1.clone());
        io3.set_channel(node2.clone(), sender2.clone());
    }
    
    let mut rlog1 = ReplicatedLog::new(node1.clone(), cluster.clone(), io1, &MetricBuilder::new()).unwrap();
    let mut rlog2 = ReplicatedLog::new(node2, cluster.clone(), io2, &MetricBuilder::new()).unwrap();
    let mut rlog3 = ReplicatedLog::new(node3, cluster.clone(), io3, &MetricBuilder::new()).unwrap();
    
    use std::io;
    let mut input = String::new();

    loop {
        input.clear();
        print!("[user input] > ");
        io::stdout().flush().unwrap();
        io::stdin().read_line(&mut input).unwrap();

        let input = input.trim();
        println!("{}", &input);

        match input {
            "a" => {
                unsafe { rlog1.io_mut().invoke_timer(); }
            }
            "ban1" => {
                unsafe {
                    rlog2.io_mut().set_ban_list(&vec!["nodeA"]);
                    rlog3.io_mut().set_ban_list(&vec!["nodeA"]);
                }
            }
            "pa" => {
                rlog1.propose_command(Vec::new());
            }
            "b" => {
                unsafe { rlog2.io_mut().invoke_timer(); }
            }
            "c" => {
                unsafe { rlog3.io_mut().invoke_timer(); }
            }
            "S" => {
                let index = rlog2.local_history().tail().index;
                dbg!(&index);
                rlog2.install_snapshot(index, Vec::new());
            }
            "prepare" => {
                unsafe {
                    rlog1.io_mut().prepare();
                }
            }
            "clear1" => {
                unsafe {
                    rlog2.io_mut().clear_ban_list();
                    rlog3.io_mut().clear_ban_list();
                }
            }
            "heartb" => {
                rlog2.heartbeat();
            }
            "reboot" => {
                io1 = rlog1.finish();

                dbg!(&io1.snapshot());
                dbg!(&io1.rawlog());
                
                rlog1 = ReplicatedLog::new(node1.clone(), cluster.clone(), io1, &MetricBuilder::new()).unwrap();
            }
            _ => {
                if input != "" {
                    panic!("wrong input: {}", input);
                }
            }
        }

        for _ in 0..50 {
            if let Async::Ready(event) = rlog1.poll().unwrap() {
                println!("[[A]] event {:?}", event);
            }

            if let Async::Ready(event) = rlog2.poll().unwrap() {
                println!("[[B]] event {:?}", event);
            }

            if let Async::Ready(event) = rlog3.poll().unwrap() {
                // println!("[[C]] event {:?}", event);
            }
        }

        {
            println!("--- A ---");
            println!("{:?}", rlog1.local_node());
            dbg!(rlog1.local_history());
            dbg!(rlog1.io().snapshot());
            dbg!(rlog1.io().rawlog());

            println!("--- B ---");
            println!("{:?}", rlog2.local_node());
            dbg!(rlog2.local_history());
            dbg!(rlog2.io().snapshot());
            dbg!(rlog2.io().rawlog());            

            /*
            println!("--- C ---");
            println!("{:?}", rlog3.local_node());
            dbg!(rlog3.local_history());
            dbg!(rlog3.io().snapshot());
            dbg!(rlog3.io().rawlog());
             */
        }
    }
    
    println!("Bye!");
}

fn main() {
    scenario2();
}
