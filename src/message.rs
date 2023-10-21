use std::{
    fmt::{self, Debug},
    marker::PhantomData,
    ops::{Deref, DerefMut},
};

use crate::{
    master::Puppeter,
    puppet::{self, Handler, Puppet},
};
use async_trait::async_trait;
#[cfg(feature = "rayon")]
use pollster::FutureExt;
#[cfg(feature = "rayon")]
use rayon;
use tokio::sync::{mpsc, oneshot};

use crate::PuppeterError;

pub trait Message: Send + 'static {}

#[derive(Debug, Clone, strum::Display)]
pub enum ServiceCommand {
    InitiateStart,
    InitiateStop,
    RequestRestart,
    ForceTermination,
    ReportFailure(Option<String>),
}

impl Message for ServiceCommand {}

pub type ReplyAddress<T> = oneshot::Sender<Result<T, PuppeterError>>;
pub type MaybeReplyAddress<T> = Option<ReplyAddress<T>>;
pub type MessageResponse<P, H> = <H as Handler<P>>::Response;

#[async_trait]
pub trait Envelope<P: Puppet>: Send {
    async fn handle_message(&mut self, puppet: &mut P) -> Result<(), PuppeterError>;
    async fn reply_error(&mut self, err: PuppeterError) -> Result<(), PuppeterError>;
}

pub struct Packet<P, M>
where
    P: Handler<M>,
    M: Message,
{
    message: Option<M>,
    reply_address: Option<oneshot::Sender<Result<P::Response, PuppeterError>>>,
    _phantom: PhantomData<P>,
}

impl<P, M> Packet<P, M>
where
    P: Handler<M>,
    M: Message,
{
    pub fn without_reply(message: M) -> Self {
        Self {
            message: Some(message),
            reply_address: None,
            _phantom: PhantomData,
        }
    }
    pub fn with_reply(
        message: M,
        reply_address: oneshot::Sender<Result<P::Response, PuppeterError>>,
    ) -> Self {
        Self {
            message: Some(message),
            reply_address: Some(reply_address),
            _phantom: PhantomData,
        }
    }
}

#[async_trait]
impl<P, M> Envelope<P> for Packet<P, M>
where
    P: Handler<M>,
    M: Message + 'static,
{
    async fn handle_message(&mut self, puppet: &mut P) -> Result<(), PuppeterError> {
        let execution_variant = puppet::execution::ExecutionVariant::from_type::<P::Exec>();
        let msg = self.message.take().unwrap();
        let reply_address = self.reply_address.take();
        match execution_variant {
            puppet::execution::ExecutionVariant::Sequential => {
                let response = puppet.handle_message(msg).await;
                if let Some(reply_address) = reply_address {
                    reply_address
                        .send(Ok(response))
                        .unwrap_or_else(|_| println!("Message response send error"));
                }
            }
            puppet::execution::ExecutionVariant::Concurrent => {
                let mut cloned_minion = puppet.clone();
                tokio::spawn(async move {
                    let response = cloned_minion.handle_message(msg).await;
                    if let Some(reply_address) = reply_address {
                        reply_address
                            .send(Ok(response))
                            .unwrap_or_else(|_| println!("Message response send error"));
                    };
                });
            }
            #[cfg(feature = "rayon")]
            puppet::execution::ExecutionVariant::Parallel => {
                let mut cloned_minion = puppet.clone();
                rayon::spawn(move || {
                    let response = cloned_minion.handle_message(msg).block_on();
                    if let Some(reply_address) = reply_address {
                        reply_address
                            .send(Ok(response))
                            .unwrap_or_else(|_| println!("Message response send error"));
                    };
                });
            }
        };
        Ok(())
    }
    async fn reply_error(&mut self, err: PuppeterError) -> Result<(), PuppeterError> {
        if let Some(reply_address) = self.reply_address.take() {
            reply_address
                .send(Err(err))
                .unwrap_or_else(|_| println!("Message response send error"));
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct Postman<A>
where
    A: Puppet,
{
    tx: tokio::sync::mpsc::Sender<Box<dyn Envelope<A>>>,
}

impl<A> Clone for Postman<A>
where
    A: Puppet,
{
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
        }
    }
}

impl<A> Postman<A>
where
    A: Puppet,
{
    pub fn new(tx: tokio::sync::mpsc::Sender<Box<dyn Envelope<A>>>) -> Self {
        Self { tx }
    }

    #[inline(always)]
    pub async fn send<E>(&self, message: E) -> Result<(), PuppeterError>
    where
        A: Handler<E>,
        E: Message + 'static,
    {
        let packet = Packet::without_reply(message);
        self.tx
            .send(Box::new(packet))
            .await
            .map_err(|_| PuppeterError::MessageSendError)?;
        Ok(())
    }

    #[inline(always)]
    pub async fn send_and_await_response<E>(&self, message: E) -> Result<A::Response, PuppeterError>
    where
        A: Handler<E>,
        E: Message + 'static,
    {
        let (res_tx, res_rx) =
            tokio::sync::oneshot::channel::<Result<A::Response, PuppeterError>>();

        let packet = Packet::with_reply(message, res_tx);
        self.tx
            .send(Box::new(packet))
            .await
            .map_err(|_| PuppeterError::MessageSendError)?;

        match res_rx.await {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(err)) => Err(err),
            Err(_) => Err(PuppeterError::MessageResponseReceiveError),
        }
    }
}

pub struct ServicePacket {
    pub(crate) cmd: ServiceCommand,
    pub(crate) reply_address: oneshot::Sender<Result<(), PuppeterError>>,
}

#[derive(Debug, Clone)]
pub(crate) struct ServicePostman {
    tx: tokio::sync::mpsc::Sender<ServicePacket>,
}

impl ServicePostman {
    pub fn new(tx: tokio::sync::mpsc::Sender<ServicePacket>) -> Self {
        Self { tx }
    }

    pub async fn send_and_await_response(
        &self,
        command: ServiceCommand,
    ) -> Result<(), PuppeterError> {
        let (res_tx, res_rx) = tokio::sync::oneshot::channel::<Result<(), PuppeterError>>();
        let packet = ServicePacket {
            cmd: command,
            reply_address: res_tx,
        };
        self.tx
            .send(packet)
            .await
            .map_err(|_| PuppeterError::MessageSendError)?;

        match res_rx.await {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(err)) => Err(err),
            Err(_) => Err(PuppeterError::MessageResponseReceiveError),
        }
    }
}

pub(crate) struct Mailbox<A>
where
    A: Puppet,
{
    rx: mpsc::Receiver<Box<dyn Envelope<A>>>,
}

impl<A: Puppet> fmt::Debug for Mailbox<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Mailbox").field("rx", &self.rx).finish()
    }
}

impl<A> Mailbox<A>
where
    A: Puppet,
{
    pub fn new(rx: mpsc::Receiver<Box<dyn Envelope<A>>>) -> Self {
        Self { rx }
    }
    pub async fn recv(&mut self) -> Option<Box<dyn Envelope<A>>>
    where
        A: Puppet,
    {
        self.rx.recv().await
    }
    pub async fn cleanup(&mut self) {
        let duration = std::time::Duration::from_millis(100);
        while let Ok(Some(_)) = tokio::time::timeout(duration, self.recv()).await {}
    }
}

#[derive(Debug)]
pub(crate) struct ServiceMailbox {
    rx: tokio::sync::mpsc::Receiver<ServicePacket>,
}

impl ServiceMailbox {
    pub fn new(rx: tokio::sync::mpsc::Receiver<ServicePacket>) -> Self {
        Self { rx }
    }
    pub async fn recv(&mut self) -> Option<ServicePacket> {
        self.rx.recv().await
    }
}
