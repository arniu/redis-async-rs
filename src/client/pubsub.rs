/*
 * Copyright 2017-2019 Ben Ashford
 *
 * Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
 * http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
 * <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
 * option. This file may not be copied, modified, or distributed
 * except according to those terms.
 */

use std::{
    collections::{btree_map::Entry, BTreeMap},
    future::Future,
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use futures_channel::{mpsc, oneshot};
use futures_sink::Sink;
use futures_util::stream::{Fuse, Stream, StreamExt};

use super::connect::{connect, RespConnection};

use crate::{
    error::{self, ConnectionReason},
    reconnect::{reconnect, Reconnect},
    resp::{self, FromResp},
};

#[derive(Debug)]
enum PubsubEvent {
    /// The: topic, sink to send messages through, and a oneshot to signal subscription has
    /// occurred.
    Subscribe(String, PubsubSink, oneshot::Sender<()>),
    /// The name of the topic to unsubscribe from. Unsubscription will be signaled by the stream
    /// closing without error.
    Unsubscribe(String),
}

type PubsubStreamInner = mpsc::UnboundedReceiver<Result<resp::RespValue, error::Error>>;
type PubsubSink = mpsc::UnboundedSender<Result<resp::RespValue, error::Error>>;

/// A spawned future that handles a Pub/Sub connection and routes messages to streams for
/// downstream consumption
struct PubsubConnectionInner {
    /// The actual Redis connection
    connection: RespConnection,
    /// A stream onto which subscription/unsubscription requests are read
    out_rx: Fuse<mpsc::UnboundedReceiver<PubsubEvent>>,
    /// Current subscriptions
    subscriptions: BTreeMap<String, PubsubSink>,
    /// Subscriptions that have not yet been confirmed
    pending_subs: BTreeMap<String, (PubsubSink, oneshot::Sender<()>)>,
    /// Any incomplete messages to be sent...
    send_pending: Option<resp::RespValue>,
}

impl PubsubConnectionInner {
    fn new(con: RespConnection, out_rx: mpsc::UnboundedReceiver<PubsubEvent>) -> Self {
        PubsubConnectionInner {
            connection: con,
            out_rx: out_rx.fuse(),
            subscriptions: BTreeMap::new(),
            pending_subs: BTreeMap::new(),
            send_pending: None,
        }
    }

    /// Returns `true` if data sent, or `false` if stream not ready...
    fn do_send(&mut self, cx: &mut Context, msg: resp::RespValue) -> Result<bool, error::Error> {
        match Pin::new(&mut self.connection).poll_ready(cx) {
            Poll::Ready(_) => {
                Pin::new(&mut self.connection).start_send(msg)?;
                Ok(true)
            }
            Poll::Pending => {
                self.send_pending = Some(msg);
                Ok(false)
            }
        }
    }

    fn do_flush(&mut self, cx: &mut Context) -> Result<(), error::Error> {
        match Pin::new(&mut self.connection).poll_flush(cx) {
            Poll::Ready(r) => r.map_err(|e| e.into()),
            Poll::Pending => Ok(()),
        }
    }

    // Returns true = flushing required.  false = no flushing required
    fn handle_new_subs(&mut self, cx: &mut Context) -> Result<(), error::Error> {
        if let Some(msg) = self.send_pending.take() {
            if !self.do_send(cx, msg)? {
                return Ok(());
            }
        }
        loop {
            match self.out_rx.poll_next_unpin(cx) {
                Poll::Pending => return Ok(()),
                Poll::Ready(None) => return Ok(()),
                Poll::Ready(Some(pubsub_event)) => {
                    let message = match pubsub_event {
                        PubsubEvent::Subscribe(topic, sender, signal) => {
                            self.pending_subs.insert(topic.clone(), (sender, signal));
                            resp_array!["SUBSCRIBE", topic]
                        }
                        PubsubEvent::Unsubscribe(topic) => resp_array!["UNSUBSCRIBE", topic],
                    };
                    if !self.do_send(cx, message)? {
                        return Ok(());
                    }
                }
            }
        }
    }

    fn handle_message(&mut self, msg: resp::RespValue) -> Result<bool, error::Error> {
        let (message_type, topic, msg) = match msg {
            resp::RespValue::Array(mut messages) => match (
                messages.pop(),
                messages.pop(),
                messages.pop(),
                messages.pop(),
            ) {
                (Some(msg), Some(topic), Some(message_type), None) => {
                    match (msg, String::from_resp(topic), message_type) {
                        (msg, Ok(topic), resp::RespValue::BulkString(bytes)) => (bytes, topic, msg),
                        _ => return Err(error::unexpected("Incorrect format of a PUBSUB message")),
                    }
                }
                _ => {
                    return Err(error::unexpected(
                        "Wrong number of parts for a PUBSUB message",
                    ));
                }
            },
            _ => {
                return Err(error::unexpected(
                    "PUBSUB message should be encoded as an array",
                ));
            }
        };

        match message_type.as_slice() {
            b"subscribe" => match self.pending_subs.remove(&topic) {
                Some((sender, signal)) => {
                    self.subscriptions.insert(topic, sender);
                    signal
                        .send(())
                        .map_err(|()| error::internal("Error confirming subscription"))?
                }
                None => {
                    return Err(error::internal(format!(
                        "Received unexpected subscribe notification for topic: {}",
                        topic
                    )));
                }
            },
            b"unsubscribe" => {
                match self.subscriptions.entry(topic) {
                    Entry::Occupied(entry) => {
                        entry.remove_entry();
                    }
                    Entry::Vacant(vacant) => {
                        return Err(error::internal(format!(
                            "Unexpected unsubscribe message: {}",
                            vacant.key()
                        )));
                    }
                }
                if self.subscriptions.is_empty() {
                    return Ok(false);
                }
            }
            b"message" => match self.subscriptions.get(&topic) {
                Some(sender) => sender.unbounded_send(Ok(msg)).expect("Cannot send message"),
                None => {
                    return Err(error::internal(format!(
                        "Unexpected message on topic: {}",
                        topic
                    )));
                }
            },
            t => {
                return Err(error::internal(format!(
                    "Unexpected data on Pub/Sub connection: {}",
                    String::from_utf8_lossy(t)
                )));
            }
        }

        Ok(true)
    }

    /// Returns true, if there are still valid subscriptions at the end, or false if not, i.e. the whole thing can be dropped.
    fn handle_messages(&mut self, cx: &mut Context) -> Result<bool, error::Error> {
        loop {
            match self.connection.poll_next_unpin(cx) {
                Poll::Pending => return Ok(true),
                Poll::Ready(None) => {
                    if self.subscriptions.is_empty() {
                        return Ok(false);
                    } else {
                        // This can only happen if the connection is closed server-side
                        for sub in self.subscriptions.values() {
                            sub.unbounded_send(Err(error::Error::Connection(
                                ConnectionReason::NotConnected,
                            )))
                            .unwrap();
                        }
                        return Err(error::Error::Connection(ConnectionReason::NotConnected));
                    }
                }
                Poll::Ready(Some(Ok(message))) => {
                    let message_result = self.handle_message(message)?;
                    if !message_result {
                        return Ok(false);
                    }
                }
                Poll::Ready(Some(Err(e))) => {
                    for sub in self.subscriptions.values() {
                        sub.unbounded_send(Err(error::unexpected(format!(
                            "Connection is in the process of failing due to: {}",
                            e
                        ))))
                        .unwrap();
                    }
                    return Err(e);
                }
            }
        }
    }
}

impl Future for PubsubConnectionInner {
    type Output = Result<(), error::Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let this_self = self.get_mut();
        this_self.handle_new_subs(cx)?;
        this_self.do_flush(cx)?;
        let cont = this_self.handle_messages(cx)?;
        if cont {
            Poll::Pending
        } else {
            Poll::Ready(Ok(()))
        }
    }
}

/// A shareable reference to subscribe to PUBSUB topics
#[derive(Debug, Clone)]
pub struct PubsubConnection {
    out_tx_c: Arc<Reconnect<PubsubEvent, mpsc::UnboundedSender<PubsubEvent>>>,
}

async fn inner_conn_fn(
    addr: SocketAddr,
) -> Result<mpsc::UnboundedSender<PubsubEvent>, error::Error> {
    let connection = connect(&addr).await?;
    let (out_tx, out_rx) = mpsc::unbounded();
    tokio::spawn(async {
        match PubsubConnectionInner::new(connection, out_rx).await {
            Ok(_) => (),
            Err(e) => log::error!("Pub/Sub error: {:?}", e),
        }
    });
    Ok(out_tx)
}

/// Used for Redis's PUBSUB functionality.
///
/// Returns a future that resolves to a `PubsubConnection`. The future will only resolve once the
/// connection is established; after the intial establishment, if the connection drops for any
/// reason (e.g. Redis server being restarted), the connection will attempt re-connect, however
/// any subscriptions will need to be re-subscribed.
pub async fn pubsub_connect(addr: &SocketAddr) -> Result<PubsubConnection, error::Error> {
    let addr = *addr;
    let reconnecting_f = reconnect(
        |con: &mpsc::UnboundedSender<PubsubEvent>, act| {
            con.unbounded_send(act).map_err(|e| e.into())
        },
        move || {
            let con_f = inner_conn_fn(addr);
            Box::new(Box::pin(con_f))
        },
    );
    Ok(PubsubConnection {
        out_tx_c: Arc::new(reconnecting_f.await?),
    })
}

impl PubsubConnection {
    /// Subscribes to a particular PUBSUB topic.
    ///
    /// Returns a future that resolves to a `Stream` that contains all the messages published on
    /// that particular topic.
    ///
    /// The resolved stream will end with `redis_async::error::Error::EndOfStream` if the
    /// underlying connection is lost for unexpected reasons. In this situation, clients should
    /// `subscribe` to re-subscribe; the underlying connect will automatically reconnect. However,
    /// clients should be aware that resubscriptions will only succeed if the underlying connection
    /// has re-established, so multiple calls to `subscribe` may be required.
    pub async fn subscribe(&self, topic: &str) -> Result<PubsubStream, error::Error> {
        let (tx, rx) = mpsc::unbounded();
        let (signal_t, signal_r) = oneshot::channel();
        self.out_tx_c
            .do_work(PubsubEvent::Subscribe(topic.to_owned(), tx, signal_t))?;

        match signal_r.await {
            Ok(_) => Ok(PubsubStream {
                topic: topic.to_owned(),
                underlying: rx,
                con: self.clone(),
            }),
            Err(_) => Err(error::internal("Subscription failed, try again later...")),
        }
    }

    /// Tells the client to unsubscribe from a particular topic. This will return immediately, the
    /// actual unsubscription will be confirmed when the stream returned from `subscribe` ends.
    pub fn unsubscribe<T: Into<String>>(&self, topic: T) {
        // Ignoring any results, as any errors communicating with Redis would de-facto unsubscribe
        // anyway, and would be reported/logged elsewhere
        let _ = self
            .out_tx_c
            .do_work(PubsubEvent::Unsubscribe(topic.into()));
    }
}

pub struct PubsubStream {
    topic: String,
    underlying: PubsubStreamInner,
    con: PubsubConnection,
}

impl Stream for PubsubStream {
    type Item = Result<resp::RespValue, error::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        self.get_mut().underlying.poll_next_unpin(cx)
    }
}

impl Drop for PubsubStream {
    fn drop(&mut self) {
        let topic: &str = self.topic.as_ref();
        self.con.unsubscribe(topic);
    }
}

#[cfg(test)]
mod test {
    use futures::{try_join, StreamExt, TryStreamExt};

    use crate::{client, resp};

    #[tokio::test]
    async fn pubsub_test() {
        let addr = "127.0.0.1:6379".parse().unwrap();
        let paired_c = client::paired_connect(&addr);
        let pubsub_c = super::pubsub_connect(&addr);
        let (paired, pubsub) = try_join!(paired_c, pubsub_c).expect("Cannot connect to Redis");

        let topic_messages = pubsub
            .subscribe("test-topic")
            .await
            .expect("Cannot subscribe to topic");

        paired.send_and_forget(resp_array!["PUBLISH", "test-topic", "test-message"]);
        paired.send_and_forget(resp_array!["PUBLISH", "test-not-topic", "test-message-1.5"]);
        let _: resp::RespValue = paired
            .send(resp_array!["PUBLISH", "test-topic", "test-message2"])
            .await
            .expect("Cannot send to topic");

        let result: Vec<_> = topic_messages
            .take(2)
            .try_collect()
            .await
            .expect("Cannot collect two values");

        assert_eq!(result.len(), 2);
        assert_eq!(result[0], "test-message".into());
        assert_eq!(result[1], "test-message2".into());
    }
}
