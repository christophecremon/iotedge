mod builder;
mod egress;
mod ingress;
mod messages;

pub use builder::Builder;
use egress::Egress;
use ingress::Ingress;
use messages::MessagesProcessor;
pub use messages::PumpMessageHandler;

use mockall::automock;
use tokio::{join, pin, select, sync::mpsc};
use tracing::{error, info};

use crate::{
    bridge::BridgeError,
    client::{EventHandler, MqttClient, MqttClientExt},
    persist::{PublicationStore, StreamWakeableState},
    settings::ConnectionSettings,
};

#[cfg(test)]
pub fn channel<M: 'static>() -> (PumpHandle<M>, mpsc::Receiver<PumpMessage<M>>) {
    let (tx, rx) = tokio::sync::mpsc::channel(10);
    (PumpHandle::new(tx), rx)
}

#[derive(Debug, thiserror::Error)]
#[error("unable to send command to pump")]
pub struct PumpError;

/// Pump is used to connect to either local broker or remote brokers
/// (including the upstream edge device)
///
/// It contains several tasks running in parallel: ingress, egress and events processing.
///
/// During `ingress` pump handles incoming MQTT publications and puts them
/// into the store. The opposite pump will read publications from a store
/// and forwards them to the corresponding broker.
///
/// During `egress` pump reads pulications from its own store and sends them
/// to the broker MQTT client connected to.
///
/// Messages processing is intended to control pump behavior: initiate pump
/// shutdown, handle configuration update or another specific event.
pub struct Pump<S, H, M>
where
    M: PumpMessageHandler,
{
    subscriptions: Vec<String>,
    messages_send: mpsc::Sender<PumpMessage<M::Message>>,
    messages: MessagesProcessor<M>,
    egress: Egress<S>,
    ingress: Ingress<H>,
}

impl<S, H, M> Pump<S, H, M>
where
    H: EventHandler,
    M: PumpMessageHandler,
    M::Message: 'static,
    S: StreamWakeableState,
{
    /// Creates a new instance of pump.
    fn new(
        messages_send: mpsc::Sender<PumpMessage<M::Message>>,
        client: MqttClient<H>,
        subscriptions: Vec<String>,
        store: PublicationStore<S>,
        messages: MessagesProcessor<M>,
    ) -> Result<Self, BridgeError> {
        let client_shutdown = client.shutdown_handle()?;
        let publish_handle = client
            .publish_handle()
            .map_err(BridgeError::PublishHandle)?;

        let egress = Egress::new(publish_handle, store);
        let ingress = Ingress::new(client, client_shutdown);

        Ok(Self {
            subscriptions,
            messages_send,
            messages,
            egress,
            ingress,
        })
    }

    /// Returns a handle to send control messages to a pump.
    pub fn handle(&self) -> PumpHandle<M::Message> {
        PumpHandle::new(self.messages_send.clone())
    }

    pub async fn subscribe(&mut self) -> Result<(), BridgeError> {
        self.ingress
            .client()
            .subscribe(&self.subscriptions) //TODO react on PumpMessage::ConfigurationUpdate instead
            .await
            .map_err(BridgeError::Subscribe)?;

        Ok(())
    }

    /// Orchestrates starting of egress, ingress and controll messages
    /// processing and waits for all of them to finish.
    ///
    /// Attempts to start all routines in the same task in parallel and
    /// waits for any of them to finish. It sends shutdown to other ones
    /// and waits until all of them stopped.
    pub async fn run(mut self) {
        info!("starting pump...");

        let shutdown_egress = self.egress.handle();
        let egress = self.egress.run();

        let shutdown_ingress = self.ingress.handle();
        let ingress = self.ingress.run();

        let shutdown_messages = self.messages.handle();
        let messages = self.messages.run();

        pin!(egress, ingress, messages);

        select! {
            _= &mut egress => {
                error!("egress stopped unexpectedly");
                shutdown_ingress.shutdown().await;
                shutdown_messages.shutdown().await;

                join!(ingress, messages);

            },
            _= &mut ingress => {
                error!("ingress stopped unexpectedly");
                shutdown_egress.shutdown().await;
                shutdown_messages.shutdown().await;

                join!(egress, messages);
            },
            _= &mut messages => {
                info!("stopping pump");
                shutdown_ingress.shutdown().await;
                shutdown_egress.shutdown().await;

                join!(egress, ingress);
            }
        };

        info!("stopped pump");
    }
}

/// A message to control pump behavior.
#[derive(Debug, PartialEq)]
pub enum PumpMessage<E> {
    Event(E),
    ConfigurationUpdate(ConnectionSettings),
    Shutdown,
}

/// A handle to send control messages to the pump.
pub struct PumpHandle<M> {
    sender: mpsc::Sender<PumpMessage<M>>,
}

#[automock]
impl<M: 'static> PumpHandle<M> {
    /// Creates a new instance of pump handle.
    fn new(sender: mpsc::Sender<PumpMessage<M>>) -> Self {
        Self { sender }
    }

    /// Sends a control message to a pump.
    pub async fn send(&mut self, message: PumpMessage<M>) -> Result<(), PumpError> {
        self.sender.send(message).await.map_err(|_| PumpError)
    }
}
