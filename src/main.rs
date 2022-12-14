use async_executor::Executor;
use async_recursion::async_recursion;
use async_std::sync::{Arc, Mutex};
use std::{collections::{HashMap, HashSet}, fmt, io};

use hex_literal::hex;
use sha2::{Digest, Sha256};

use log::{info, warn};
use rand::rngs::OsRng;
use smol::future;
use structopt::StructOpt;
use structopt_toml::StructOptToml;

use darkfi::{
    async_daemonize, net,
    net::P2pPtr,
    rpc::server::listen_and_serve,
    system::{Subscriber, SubscriberPtr},
    util::{
        cli::{get_log_config, get_log_level, spawn_config},
        expand_path,
        file::save_json_file,
        path::get_config_path,
        serial::{Decodable, Encodable, ReadExt, SerialDecodable, SerialEncodable, WriteExt},
        sleep,
    },
    Result,
};

type EventId = [u8; 32];

#[derive(SerialEncodable, SerialDecodable)]
struct Event {
    previous_event_hash: EventId,
    action: EventAction,
    timestamp: u64,
}

impl Event {
    fn hash(&self) -> EventId {
        let mut bytes = Vec::new();
        self.encode(&mut bytes).expect("serialize failed!");

        let mut hasher = Sha256::new();
        hasher.update(bytes);
        let bytes = hasher.finalize().to_vec();
        let mut result = [0u8; 32];
        result.copy_from_slice(bytes.as_slice());
        result
    }
}

impl fmt::Debug for Event {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.action {
            EventAction::PrivMsg(event) => write!(
                f,
                "PRIVMSG {}: {} ({})",
                event.nick, event.msg, self.timestamp
            ),
        }
    }
}

enum EventAction {
    PrivMsg(PrivMsgEvent),
}

impl Encodable for EventAction {
    fn encode<S: io::Write>(&self, mut s: S) -> Result<usize> {
        match self {
            Self::PrivMsg(event) => {
                let mut len = 0;
                len += 0u8.encode(&mut s)?;
                len += event.encode(s)?;
                Ok(len)
            }
        }
    }
}

impl Decodable for EventAction {
    fn decode<D: io::Read>(mut d: D) -> Result<Self> {
        let type_id = d.read_u8()?;
        match type_id {
            0 => Ok(Self::PrivMsg(PrivMsgEvent::decode(d)?)),
            _ => Err(darkfi::Error::ParseFailed("Bad type ID byte for Event")),
        }
    }
}

#[derive(SerialEncodable, SerialDecodable)]
struct PrivMsgEvent {
    nick: String,
    msg: String,
}

struct EventNode {
    // Only current root has this set to None
    parent: Option<EventNodePtr>,
    event: Event,
    children: Mutex<Vec<EventNodePtr>>,
}

type EventNodePtr = Arc<EventNode>;

struct Model {
    // This is periodically updated so we discard old nodes
    current_root: EventId,
    orphans: Vec<Event>,
    event_map: HashMap<EventId, EventNodePtr>,
}

impl Model {
    fn new() -> Self {
        let root_node = Arc::new(EventNode {
            parent: None,
            event: Event {
                previous_event_hash: [0u8; 32],
                action: EventAction::PrivMsg(PrivMsgEvent {
                    nick: "root".to_string(),
                    msg: "Let there be dark".to_string(),
                }),
                timestamp: get_current_time(),
            },
            children: Mutex::new(Vec::new()),
        });
        let root_node_id = root_node.event.hash();

        let event_map = HashMap::from([(root_node_id.clone(), root_node)]);

        Self {
            current_root: root_node_id,
            orphans: Vec::new(),
            event_map,
        }
    }

    async fn add(&mut self, event: Event) {
        self.orphans.push(event);
        self.reorganize().await;
    }

    // TODO: Update root only after some time
    // Recursively free nodes climbing up from old root to new root
    // Also remove entries from event_map

    async fn reorganize(&mut self) {
        let mut remaining_orphans = Vec::new();
        for orphan in std::mem::take(&mut self.orphans) {
            let prev_event = orphan.previous_event_hash.clone();

            // Parent does not yet exist
            if !self.event_map.contains_key(&prev_event) {
                remaining_orphans.push(orphan);

                // BIGTODO #1:
                // TODO: We need to fetch missing ancestors from the network
                // Trigger get_blocks() request

                continue;
            }

            let parent = self
                .event_map
                .get(&prev_event)
                .expect("logic error")
                .clone();
            let node = Arc::new(EventNode {
                parent: Some(parent.clone()),
                event: orphan,
                children: Mutex::new(Vec::new()),
            });

            // BIGTODO #2:
            // Reject events which attach to forks too low in the chain
            // At some point we ignore all events from old branches
            //let depth = self.find_ancestor_depth(node.clone(), self.find_head().await);
            //if depth > 10 {
            //    // Discard
            //    continue;
            //}

            parent.children.lock().await.push(node.clone());
            // Add node to the table
            self.event_map.insert(node.event.hash(), node);
        }
    }

    fn get_root(&self) -> EventNodePtr {
        let root_id = &self.current_root;
        return self
            .event_map
            .get(root_id)
            .expect("root ID is not in the event map!")
            .clone();
    }

    // find_head
    // -> recursively call itself
    // -> + 1 for every recursion, return self if no children
    // -> select max from returned values
    // Gets the lead node with the maximal number of events counting from root
    async fn find_head(&self) -> EventNodePtr {
        let root = self.get_root();
        Self::find_longest_chain(root, 0).await.1
    }

    #[async_recursion]
    async fn find_longest_chain(parent_node: EventNodePtr, i: u32) -> (u32, EventNodePtr) {
        let children = parent_node.children.lock().await;
        if children.is_empty() {
            return (i, parent_node.clone());
        }
        let mut current_max = 0;
        let mut current_node = None;
        for node in &*children {
            let (grandchild_i, grandchild_node) =
                Self::find_longest_chain(node.clone(), i + 1).await;

            if grandchild_i > current_max {
                current_max = grandchild_i;
                current_node = Some(grandchild_node.clone());
            } else if grandchild_i == current_max {
                // Break ties using the timestamp
                if grandchild_node.event.timestamp
                    > current_node
                        .as_ref()
                        .expect("current_node should be set!")
                        .event
                        .timestamp
                {
                    current_max = grandchild_i;
                    current_node = Some(grandchild_node.clone());
                }
            }
        }
        assert_ne!(current_max, 0);
        (current_max, current_node.expect("internal logic error"))
    }

    fn find_height(&self, mut node: EventNodePtr) -> u32 {
        let mut height = 0;
        while node.event.hash() != self.current_root {
            height += 1;
            node = node.parent.as_ref().expect("non-root nodes should have a parent set").clone();
        }
        height
    }

    fn find_ancestor_depth(&self, mut node_a: EventNodePtr, mut node_b: EventNodePtr) -> u32 {
        let mut depth = 0;
        while node_a.event.hash() != node_b.event.hash() {
            depth += 1;
            node_a = node_a.parent.as_ref().expect("non-root nodes should have a parent set").clone();
            node_b = node_b.parent.as_ref().expect("non-root nodes should have a parent set").clone();
        }
        depth
    }

    async fn debug(&self) {
        for (event_id, event_node) in &self.event_map {
            let height = self.find_height(event_node.clone());
            println!("{}: {:?} [height={}]", hex::encode(&event_id), event_node.event, height);
        }

        println!("root: {}", hex::encode(&self.get_root().event.hash()));
        println!(
            "head: {}",
            hex::encode(&self.find_head().await.event.hash())
        );
    }
}

pub const CONFIG_FILE: &str = "ircd_config.toml";
pub const CONFIG_FILE_CONTENTS: &str = include_str!("../ircd_config.toml");

#[derive(Clone, Debug, serde::Deserialize, StructOpt, StructOptToml)]
#[serde(default)]
#[structopt(name = "ircd")]
pub struct Args {
    #[structopt(long)]
    pub config: Option<String>,

    /// Increase verbosity
    #[structopt(short, parse(from_occurrences))]
    pub verbose: u8,
}

fn get_current_time() -> u64 {
    let start = std::time::SystemTime::now();
    start
        .duration_since(std::time::UNIX_EPOCH)
        .expect("Time went backwards")
        .as_millis()
        .try_into()
        .unwrap()
}

fn create_message(previous_event_hash: EventId, nick: &str, msg: &str, timestamp: u64) -> Event {
    Event {
        previous_event_hash,
        action: EventAction::PrivMsg(PrivMsgEvent {
            nick: nick.to_string(),
            msg: msg.to_string(),
        }),
        timestamp,
    }
}

struct View {
    seen: HashSet<EventId>,
}

impl View {
    fn new() -> Self {
        Self {
            seen: HashSet::new()
        }
    }

    fn process(model: &Model) {
        // This does 2 passes:
        // 1. Walk down all chains and get unseen events
        // 2. Order those events according to timestamp
        // Then the events are replayed to the IRC client
    }
}

async_daemonize!(realmain);
async fn realmain(settings: Args, executor: Arc<Executor<'_>>) -> Result<()> {
    let mut model = Model::new();
    let root_id = model.get_root().event.hash();

    let timestamp = get_current_time() + 1;

    let node1 = create_message(root_id, "alice", "alice message", timestamp);
    model.add(node1).await;
    let node2 = create_message(root_id, "bob", "bob message", timestamp);
    let node2_id = node2.hash();
    model.add(node2).await;
    let node3 = create_message(root_id, "charlie", "charlie message", timestamp);
    let node3_id = node3.hash();
    model.add(node3).await;

    let node4 = create_message(node2_id, "delta", "delta message", timestamp);
    let node4_id = node4.hash();
    model.add(node4).await;

    assert_eq!(model.find_head().await.event.hash(), node4_id);

    // Now lets extend another chain
    let node5 = create_message(node3_id, "epsilon", "epsilon message", timestamp);
    let node5_id = node5.hash();
    model.add(node5).await;
    let node6 = create_message(node5_id, "phi", "phi message", timestamp);
    let node6_id = node6.hash();
    model.add(node6).await;

    assert_eq!(model.find_head().await.event.hash(), node6_id);

    model.debug().await;

    Ok(())
}
