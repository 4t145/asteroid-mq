//! TOPIC 主题
//!
//!
//!

pub mod config;
pub mod durable_message;
pub mod hold_message;
pub mod wait_ack;

use std::{
    borrow::Borrow,
    collections::{HashMap, HashSet},
    hash::Hash,
    ops::Deref,
    sync::{Arc, RwLock, Weak},
    task::Poll,
};

use bytes::Bytes;
use config::TopicConfig;
use crossbeam::sync::ShardedLock;
use durable_message::{DurabilityService, DurableMessage, LoadTopic, UnloadTopic};
use hold_message::{HoldMessage, MessagePollContext, MessageQueue};
use serde::{Deserialize, Serialize};
use tracing::instrument;
use wait_ack::{WaitAck, WaitAckError, WaitAckErrorException, WaitAckHandle};

use crate::{
    impl_codec,
    protocol::{endpoint::LocalEndpointInner, node::raft::LogEntry},
    TimestampSec,
};

use super::{
    codec::CodecType,
    endpoint::{
        DelegateMessage, EndpointAddr, EndpointOffline, EndpointOnline, EpInfo, LocalEndpoint,
        LocalEndpointRef, Message, MessageId, MessageStateUpdate, MessageStatusKind,
        MessageTargetKind,
    },
    interest::{Interest, InterestMap, Subject},
    node::{
        event::{EventKind, N2nEvent, N2nPacket},
        Node, NodeId,
    },
};
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]

/// code are expect to be a valid utf8 string
pub struct TopicCode(Bytes);
impl TopicCode {
    pub fn new<B: Into<String>>(code: B) -> Self {
        Self(Bytes::from(code.into()))
    }
    pub const fn const_new(code: &'static str) -> Self {
        Self(Bytes::from_static(code.as_bytes()))
    }
}

impl Borrow<[u8]> for TopicCode {
    fn borrow(&self) -> &[u8] {
        &self.0
    }
}

impl std::fmt::Display for TopicCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        unsafe { f.write_str(std::str::from_utf8_unchecked(&self.0)) }
    }
}
impl CodecType for TopicCode {
    fn decode(bytes: Bytes) -> Result<(Self, Bytes), super::codec::DecodeError> {
        Bytes::decode(bytes).and_then(|(s, bytes)| {
            std::str::from_utf8(&s)
                .map_err(|e| super::codec::DecodeError::new::<TopicCode>(e.to_string()))?;
            Ok((TopicCode(s), bytes))
        })
    }

    fn encode(&self, buf: &mut bytes::BytesMut) {
        self.0.encode(buf)
    }
}

#[derive(Debug, Clone)]
pub struct Topic {
    pub node: Node,
    pub(crate) inner: Arc<TopicData>,
}
impl Topic {
    pub async fn send_message(&self, message: Message) -> Result<WaitAckHandle, crate::Error> {
        let handle = self.wait_ack(message.id());
        self.node
            .commit_log(LogEntry::delegate_message(DelegateMessage {
                topic: self.code().clone(),
                message,
            }))
            .await
            .map_err(crate::Error::contextual("send message"))?;
        Ok(handle)
    }
    pub fn node(&self) -> Node {
        self.node.clone()
    }
}
impl Deref for Topic {
    type Target = TopicData;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl TopicData {
    pub fn code(&self) -> &TopicCode {
        &self.config.code
    }

    pub(crate) fn get_ep_sync(&self) -> Vec<EpInfo> {
        let ep_interest_map = self.ep_interest_map;
        let ep_latest_active = self.ep_latest_active;
        let mut eps = Vec::new();
        for (ep, host) in self.ep_routing_table.iter() {
            if let Some(latest_active) = ep_latest_active.get(ep) {
                eps.push(EpInfo {
                    addr: *ep,
                    host: *host,
                    interests: ep_interest_map
                        .interest_of(ep)
                        .map(|s| s.iter().cloned().collect())
                        .unwrap_or_default(),
                    latest_active: *latest_active,
                });
            }
        }
        eps
    }
    pub(crate) fn load_ep_sync(&self, infos: Vec<EpInfo>) {
        let mut active_wg = self.ep_latest_active;
        let mut routing_wg = self.ep_routing_table;
        let mut interest_wg = self.ep_interest_map;
        for ep in infos {
            if let Some(existed_record) = active_wg.get(&ep.addr) {
                if *existed_record > ep.latest_active {
                    continue;
                }
            }
            active_wg.insert(ep.addr, ep.latest_active);
            routing_wg.insert(ep.addr, ep.host);
            for interest in &ep.interests {
                interest_wg.insert(interest.clone(), ep.addr);
            }
        }
    }

    pub(crate) fn collect_addr_by_subjects<'i>(
        &self,
        subjects: impl Iterator<Item = &'i Subject>,
    ) -> HashSet<EndpointAddr> {
        let mut ep_collect = HashSet::new();
        let rg = self.ep_interest_map;
        for subject in subjects {
            ep_collect.extend(rg.find(subject));
        }
        ep_collect
    }
    pub(crate) fn get_local_ep(&self, ep: &EndpointAddr) -> Option<LocalEndpointRef> {
        self.local_endpoints.get(ep).cloned()
    }
    pub(crate) fn push_message_to_local_ep(
        &self,
        ep: &EndpointAddr,
        message: Message,
    ) -> Result<(), Message> {
        if let Some(ep) = self.get_local_ep(ep) {
            if let Some(sender) = ep.upgrade() {
                sender.push_message(message);
                return Ok(());
            }
        }
        Err(message)
    }
    pub(crate) fn resolve_node_ep_map(
        &self,
        ep_list: impl Iterator<Item = EndpointAddr>,
    ) -> HashMap<NodeId, Vec<EndpointAddr>> {
        let rg_routing = self.ep_routing_table;
        let mut resolve_map = <HashMap<NodeId, Vec<EndpointAddr>>>::new();
        for ep in ep_list {
            if let Some(node) = rg_routing.get(&ep) {
                resolve_map.entry(*node).or_default().push(ep);
            }
        }
        resolve_map
    }
}
impl Topic {
    pub fn wait_ack(&self, message_id: MessageId) -> WaitAckHandle {
        let queue = self.queue;
        queue.wait_ack(message_id)
    }
    pub fn reference(&self) -> TopicRef {
        TopicRef {
            node: self.node.clone(),
            inner: Arc::downgrade(&self.inner),
        }
    }

    pub async fn create_endpoint(
        &mut self,
        interests: impl IntoIterator<Item = Interest>,
    ) -> Result<LocalEndpoint, crate::Error> {
        let channel = flume::unbounded();
        let topic_code = self.code().clone();
        let ep = LocalEndpoint {
            inner: Arc::new(LocalEndpointInner {
                attached_node: self.node.node_ref(),
                address: EndpointAddr::new_snowflake(),
                mail_box: channel.1,
                mail_addr: channel.0,
                interest: interests.into_iter().collect(),
                topic_code: topic_code.clone(),
                attached_topic: self.reference(),
            }),
        };
        let _ = self
            .node
            .commit_log(LogEntry::ep_online(EndpointOnline {
                topic_code: topic_code.clone(),
                endpoint: ep.address,
                interests: ep.interest.clone(),
                host: self.node.id(),
            }))
            .await
            .map_err(crate::Error::contextual("create endpoint"))?;
        self.local_endpoints.insert(ep.address, ep.reference());
        Ok(ep)
    }
    pub fn delete_endpoint(&self, addr: EndpointAddr) {
        {
            let mut local_endpoints = self.local_endpoints;

            let mut ep_interest_map = self.ep_interest_map;
            let mut ep_routing_table = self.ep_routing_table;
            let mut ep_latest_active = self.ep_latest_active;
            ep_interest_map.delete(&addr);
            ep_routing_table.remove(&addr);
            ep_latest_active.remove(&addr);
            local_endpoints.remove(&addr);
        }
        let ep_offline = EndpointOffline {
            endpoint: addr,
            host: self.node.id(),
            topic_code: self.code().clone(),
        };
        let payload = ep_offline.encode_to_bytes();
        for peer in self.node.known_peer_cluster() {
            let packet = N2nPacket::event(N2nEvent {
                to: peer,
                trace: self.node.new_trace(),
                kind: EventKind::EpOffline,
                payload: payload.clone(),
            });
            let _ = self.node.send_packet(packet, peer);
        }
    }

    pub(crate) fn dispatch_message(
        &self,
        message: &Message,
        ep_list: impl Iterator<Item = EndpointAddr>,
    ) -> Vec<(EndpointAddr, Result<(), ()>)> {
        let map = self.resolve_node_ep_map(ep_list);
        tracing::debug!(?map, "dispatch message");
        let mut results = vec![];
        for (node, eps) in map {
            if self.node.is(node) {
                for ep in &eps {
                    match self.push_message_to_local_ep(ep, message.clone()) {
                        Ok(_) => {
                            results.push((*ep, Ok(())));
                        }
                        Err(_) => {
                            results.push((*ep, Err(())));
                        }
                    }
                }
            }
        }
        results
    }

    #[instrument(skip(self, message), fields(node_id=?self.node.id(), topic_code=?self.config.code))]
    pub(crate) fn hold_new_message(&self, message: Message) {
        let ep_collect = match message.header.target_kind {
            MessageTargetKind::Durable | MessageTargetKind::Online => {
                self.collect_addr_by_subjects(message.header.subjects.iter())
                // just accept all
            }
            MessageTargetKind::Available => {
                unimplemented!("available kind is not supported");
                // unsupported
            }
            MessageTargetKind::Push => {
                let message_hash = crate::util::hash64(&message.id());
                let ep_collect = self.collect_addr_by_subjects(message.header.subjects.iter());

                let mut hash_ring = ep_collect
                    .iter()
                    .map(|ep| (crate::util::hash64(ep), *ep))
                    .collect::<Vec<_>>();
                hash_ring.sort_by_key(|x| x.0);
                if hash_ring.is_empty() {
                    let queue = self.queue;
                    if let Some(report) = queue.waiting.remove(&message.id()) {
                        let _ = report.send(Err(WaitAckError::exception(
                            WaitAckErrorException::NoAvailableTarget,
                        )));
                    }
                    return;
                } else {
                    let ep = hash_ring[(message_hash as usize) % (hash_ring.len())].1;
                    tracing::debug!(?ep, "select ep");
                    HashSet::from([ep])
                }
            }
        };
        let hold_message = HoldMessage {
            message: message.clone(),
            wait_ack: WaitAck::new(message.ack_kind(), ep_collect.clone()),
        };
        {
            let mut queue = self.queue;
            // put in queue
            if let Some(overflow_config) = &self.config.overflow_config {
                let size = u32::from(overflow_config.size) as usize;
                let waiting_size = queue.len();
                if waiting_size >= size {
                    match overflow_config.policy {
                        config::TopicOverflowPolicy::RejectNew => {
                            if let Some(report) =
                                queue.waiting.get_mut().unwrap().remove(&message.id())
                            {
                                let _ = report.send(Err(WaitAckError::exception(
                                    WaitAckErrorException::Overflow,
                                )));
                            }
                            return;
                        }
                        config::TopicOverflowPolicy::DropOld => {
                            let old = queue.pop().expect("queue at least one element");
                            if let Some(report) =
                                queue.waiting.get_mut().unwrap().remove(&old.message.id())
                            {
                                let _ = report.send(Err(WaitAckError::exception(
                                    WaitAckErrorException::Overflow,
                                )));
                            }
                        }
                    }
                }
            }
            queue.push(hold_message);
        }
        self.update_and_flush(MessageStateUpdate::new_empty(message.id()));
        tracing::debug!(?ep_collect, "hold new message");
    }

    pub(crate) fn ep_online(&self, endpoint: EndpointAddr, interests: Vec<Interest>, host: NodeId) {
        let mut message_need_poll = HashSet::new();
        {
            let mut routing_wg = self.ep_routing_table;
            let mut interest_wg = self.ep_interest_map;
            let mut active_wg = self.ep_latest_active;

            active_wg.insert(endpoint, TimestampSec::now());
            routing_wg.insert(endpoint, host);
            for interest in &interests {
                interest_wg.insert(interest.clone(), endpoint);
            }
            let queue = self.queue;
            for (id, message) in &queue.hold_messages {
                if message.message.header.target_kind == MessageTargetKind::Durable {
                    let mut status = message.wait_ack.status;

                    if !status.contains_key(&endpoint)
                        && message
                            .message
                            .header
                            .subjects
                            .iter()
                            .any(|s| interest_wg.find(s).contains(&endpoint))
                    {
                        status.insert(endpoint, MessageStatusKind::Unsent);
                        message_need_poll.insert(*id);
                    }
                }
            }
        }
        for id in message_need_poll {
            self.update_and_flush(MessageStateUpdate::new_empty(id));
        }
    }

    pub(crate) fn ep_offline(&self, ep: &EndpointAddr) {
        let mut routing_wg = self.ep_routing_table;
        let mut interest_wg = self.ep_interest_map;
        let mut active_wg = self.ep_latest_active;
        active_wg.remove(ep);
        routing_wg.remove(ep);
        interest_wg.delete(ep);
    }

    pub(crate) fn update_ep_interest(&self, ep: &EndpointAddr, interests: Vec<Interest>) {
        let mut interest_wg = self.ep_interest_map;
        interest_wg.delete(ep);
        for interest in interests {
            interest_wg.insert(interest, *ep);
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct TopicRef {
    node: Node,
    inner: Weak<TopicData>,
}

impl TopicRef {
    pub fn upgrade(&self) -> Option<Topic> {
        self.inner.upgrade().map(|inner| Topic {
            node: self.node.clone(),
            inner,
        })
    }
}

#[derive(Debug, Clone)]
pub struct TopicData {
    pub(crate) config: TopicConfig,
    pub(crate) ep_routing_table: HashMap<EndpointAddr, NodeId>,
    pub(crate) ep_interest_map: InterestMap<EndpointAddr>,
    pub(crate) ep_latest_active: HashMap<EndpointAddr, TimestampSec>,
    pub(crate) queue: MessageQueue,
}

impl TopicData {
    pub(crate) fn from_snapshot(snapshot: TopicSnapshot) -> Self {
        let TopicSnapshot {
            config,
            ep_routing_table,
            ep_interest_map,
            ep_latest_active,
            mut queue,
        } = snapshot;
        let mut topic = TopicData::new(config);
        queue.sort_by_key(|m|m.time);
        for message in queue {
            topic.queue.push(HoldMessage::from_durable(message));
        }
        Self {
            config,
            ep_routing_table,
            ep_interest_map: InterestMap::from_raw(ep_interest_map),
            ep_latest_active,
            queue: MessageQueue::new(queue),
        }
    }
    pub(crate) fn update_and_flush(&mut self, update: MessageStateUpdate) {
        let poll_result = {
            for (from, status) in update.status {
                self.queue.update_ack(&update.message_id, from, status)
            }
            self.queue
                .poll_message(update.message_id, &MessagePollContext { topic: self })
        };
        if let Some(Poll::Ready(())) = poll_result {
            self.queue.flush(&MessagePollContext { topic: self });
        }
    }
    pub fn hold_new_message(&mut self, message: Message) {
        let ep_collect = match message.header.target_kind {
            MessageTargetKind::Durable | MessageTargetKind::Online => {
                self.collect_addr_by_subjects(message.header.subjects.iter())
                // just accept all
            }
            MessageTargetKind::Available => {
                unimplemented!("available kind is not supported");
                // unsupported
            }
            MessageTargetKind::Push => {
                let message_hash = crate::util::hash64(&message.id());
                let ep_collect = self.collect_addr_by_subjects(message.header.subjects.iter());

                let mut hash_ring = ep_collect
                    .iter()
                    .map(|ep| (crate::util::hash64(ep), *ep))
                    .collect::<Vec<_>>();
                hash_ring.sort_by_key(|x| x.0);
                if hash_ring.is_empty() {
                    let queue = self.queue;
                    if let Some(report) = queue.waiting.remove(&message.id()) {
                        let _ = report.send(Err(WaitAckError::exception(
                            WaitAckErrorException::NoAvailableTarget,
                        )));
                    }
                    return;
                } else {
                    let ep = hash_ring[(message_hash as usize) % (hash_ring.len())].1;
                    tracing::debug!(?ep, "select ep");
                    HashSet::from([ep])
                }
            }
        };
        let hold_message = HoldMessage {
            message: message.clone(),
            wait_ack: WaitAck::new(message.ack_kind(), ep_collect.clone()),
        };
        {
            let mut queue = self.queue;
            // put in queue
            if let Some(overflow_config) = &self.config.overflow_config {
                let size = u32::from(overflow_config.size) as usize;
                let waiting_size = queue.len();
                if waiting_size >= size {
                    match overflow_config.policy {
                        config::TopicOverflowPolicy::RejectNew => {
                            if let Some(report) = queue.waiting.remove(&message.id()) {
                                let _ = report.send(Err(WaitAckError::exception(
                                    WaitAckErrorException::Overflow,
                                )));
                            }
                            return;
                        }
                        config::TopicOverflowPolicy::DropOld => {
                            let old = queue.pop().expect("queue at least one element");
                            if let Some(report) = queue.waiting.remove(&old.message.id()) {
                                let _ = report.send(Err(WaitAckError::exception(
                                    WaitAckErrorException::Overflow,
                                )));
                            }
                        }
                    }
                }
            }
            queue.push(hold_message);
        }
        self.update_and_flush(MessageStateUpdate::new_empty(message.id()));
        tracing::debug!(?ep_collect, "hold new message");
    }
}

#[derive(Debug, Clone)]

pub struct TopicSnapshot {
    pub config: TopicConfig,
    pub ep_routing_table: HashMap<EndpointAddr, NodeId>,
    pub ep_interest_map: HashMap<EndpointAddr, HashSet<Interest>>,
    pub ep_latest_active: HashMap<EndpointAddr, TimestampSec>,
    pub queue: Vec<DurableMessage>,
}


impl Topic {
    pub(crate) fn apply_snapshot(&mut self, snapshot: TopicSnapshot) {
        {
            self.ep_routing_table = snapshot.ep_routing_table;
            self.ep_interest_map = InterestMap::from_raw(snapshot.ep_interest_map);
            self.ep_latest_active = snapshot.ep_latest_active;
            self.queue.clear();
            for message in snapshot.queue {
                self.queue.push(HoldMessage::from_durable(message));
            }
            self.queue.poll_all(&context);
            self.queue.flush(&context);
        }
    }
}
impl TopicData {
    pub(crate) fn snapshot(&self) -> TopicSnapshot {
        let ep_routing_table = self.ep_routing_table.clone();
        let ep_interest_map = self.ep_interest_map.raw.clone();
        let ep_latest_active = self.ep_latest_active.clone();
        let queue = self
            .queue
            .hold_messages
            .values()
            .map(|m| m.as_durable())
            .collect();
        TopicSnapshot {
            config: self.config.clone(),
            ep_routing_table,
            ep_interest_map,
            ep_latest_active,
            queue,
        }
    }

    pub(crate) fn new<C: Into<TopicConfig>>(config: C) -> Self {
        const DEFAULT_CAPACITY: usize = 128;
        let config: TopicConfig = config.into();
        let capacity = if let Some(ref overflow_config) = config.overflow_config {
            overflow_config.size()
        } else {
            DEFAULT_CAPACITY
        };
        let messages = MessageQueue::new(config.blocking, capacity);
        Self {
            config,
            local_endpoints: Default::default(),
            ep_routing_table: Default::default(),
            ep_interest_map: Default::default(),
            ep_latest_active: Default::default(),
            queue: messages,
        }
    }
}

impl Node {
    pub async fn new_topic<C: Into<TopicConfig>>(&self, config: C) -> Result<Topic, crate::Error> {
        self.load_topic(LoadTopic::from_config(config)).await
    }
    pub async fn load_topic(&self, load_topic: LoadTopic) -> Result<Topic, crate::Error> {
        let code = load_topic.config.code.clone();
        let is_leader = self.wait_raft_cluster_ready().await;
        let topic = if is_leader {
            self.commit_log(LogEntry::load_topic(load_topic))
                .await
                .map_err(crate::Error::contextual("new topic"))?;
            self.get_topic(&code).expect("topic should be loaded")
        } else {
            loop {
                if let Some(topic) = self.get_topic(&code) {
                    break topic;
                }
                tokio::task::yield_now().await;
            }
        };
        Ok(topic)
    }
    pub async fn delete_topic(&self, code: TopicCode) {
        let is_leader = self.wait_raft_cluster_ready().await;
        if is_leader {
            self.commit_log(LogEntry::unload_topic(UnloadTopic::new(code)))
                .await
                .expect("cancel topic");
        }
    }

    pub fn remove_topic<Q>(&self, code: &Q)
    where
        TopicCode: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        if let Some(topic) = self.topics.remove(code) {
            let mut queue = topic.queue;
            let waitings = queue.waiting.get_mut().unwrap();
            for (_, report) in waitings.drain() {
                let _ = report.send(Err(WaitAckError::exception(
                    WaitAckErrorException::MessageDropped,
                )));
            }
            queue.clear();
        }
    }
    pub(crate) fn wrap_topic(&self, topic_inner: Arc<TopicData>) -> Topic {
        Topic {
            node: self.clone(),
            inner: topic_inner.clone(),
        }
    }
    pub fn get_topic<Q>(&self, code: &Q) -> Option<Topic>
    where
        TopicCode: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.topics
            .read()
            .unwrap()
            .get(code)
            .map(|t| self.wrap_topic(t.clone()))
    }
}
