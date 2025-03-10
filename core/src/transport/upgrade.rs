// Copyright 2017-2019 Parity Technologies (UK) Ltd.
//
// Permission is hereby granted, free of charge, to any person obtaining a
// copy of this software and associated documentation files (the "Software"),
// to deal in the Software without restriction, including without limitation
// the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the
// Software is furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS
// OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
// FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
// DEALINGS IN THE SOFTWARE.

//! Configuration of transport protocol upgrades.

use std::{
    error::Error,
    fmt,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

use futures::{prelude::*, ready};
use libp2p_identity::PeerId;
use multiaddr::Multiaddr;

pub use crate::upgrade::Version;
use crate::{
    connection::ConnectedPoint,
    muxing::{StreamMuxer, StreamMuxerBox},
    transport::{
        and_then::AndThen, boxed::boxed, timeout::TransportTimeout, DialOpts, ListenerId,
        Transport, TransportError, TransportEvent,
    },
    upgrade::{
        self, apply_inbound, apply_outbound, InboundConnectionUpgrade, InboundUpgradeApply,
        OutboundConnectionUpgrade, OutboundUpgradeApply, UpgradeError,
    },
    Negotiated,
};

/// A `Builder` facilitates upgrading of a [`Transport`] for use with
/// a `Swarm`.
///
/// The upgrade process is defined by the following stages:
///
///    [`authenticate`](Builder::authenticate)`{1}`
/// -> [`apply`](Authenticated::apply)`{*}`
/// -> [`multiplex`](Authenticated::multiplex)`{1}`
///
/// It thus enforces the following invariants on every transport
/// obtained from [`multiplex`](Authenticated::multiplex):
///
///   1. The transport must be [authenticated](Builder::authenticate) and
///      [multiplexed](Authenticated::multiplex).
///   2. Authentication must precede the negotiation of a multiplexer.
///   3. Applying a multiplexer is the last step in the upgrade process.
///   4. The [`Transport::Output`] conforms to the requirements of a `Swarm`, namely a tuple of a
///      [`PeerId`] (from the authentication upgrade) and a [`StreamMuxer`] (from the multiplexing
///      upgrade).
#[derive(Clone)]
pub struct Builder<T> {
    inner: T,
    version: upgrade::Version,
}

impl<T> Builder<T>
where
    T: Transport,
    T::Error: 'static,
{
    /// Creates a `Builder` over the given (base) `Transport`.
    pub fn new(inner: T, version: upgrade::Version) -> Builder<T> {
        Builder { inner, version }
    }

    /// Upgrades the transport to perform authentication of the remote.
    ///
    /// The supplied upgrade receives the I/O resource `C` and must
    /// produce a pair `(PeerId, D)`, where `D` is a new I/O resource.
    /// The upgrade must thus at a minimum identify the remote, which typically
    /// involves the use of a cryptographic authentication protocol in the
    /// context of establishing a secure channel.
    ///
    /// ## Transitions
    ///
    ///   * I/O upgrade: `C -> (PeerId, D)`.
    ///   * Transport output: `C -> (PeerId, D)`
    pub fn authenticate<C, D, U, E>(
        self,
        upgrade: U,
    ) -> Authenticated<AndThen<T, impl FnOnce(C, ConnectedPoint) -> Authenticate<C, U> + Clone>>
    where
        T: Transport<Output = C>,
        C: AsyncRead + AsyncWrite + Unpin,
        D: AsyncRead + AsyncWrite + Unpin,
        U: InboundConnectionUpgrade<Negotiated<C>, Output = (PeerId, D), Error = E>,
        U: OutboundConnectionUpgrade<Negotiated<C>, Output = (PeerId, D), Error = E> + Clone,
        E: Error + 'static,
    {
        let version = self.version;
        Authenticated(Builder::new(
            self.inner.and_then(move |conn, endpoint| Authenticate {
                inner: upgrade::apply(conn, upgrade, endpoint, version),
            }),
            version,
        ))
    }
}

/// An upgrade that authenticates the remote peer, typically
/// in the context of negotiating a secure channel.
///
/// Configured through [`Builder::authenticate`].
#[pin_project::pin_project]
pub struct Authenticate<C, U>
where
    C: AsyncRead + AsyncWrite + Unpin,
    U: InboundConnectionUpgrade<Negotiated<C>> + OutboundConnectionUpgrade<Negotiated<C>>,
{
    #[pin]
    inner: EitherUpgrade<C, U>,
}

impl<C, U> Future for Authenticate<C, U>
where
    C: AsyncRead + AsyncWrite + Unpin,
    U: InboundConnectionUpgrade<Negotiated<C>>
        + OutboundConnectionUpgrade<
            Negotiated<C>,
            Output = <U as InboundConnectionUpgrade<Negotiated<C>>>::Output,
            Error = <U as InboundConnectionUpgrade<Negotiated<C>>>::Error,
        >,
{
    type Output = <EitherUpgrade<C, U> as Future>::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        Future::poll(this.inner, cx)
    }
}

/// An upgrade that negotiates a (sub)stream multiplexer on
/// top of an authenticated transport.
///
/// Configured through [`Authenticated::multiplex`].
#[pin_project::pin_project]
pub struct Multiplex<C, U>
where
    C: AsyncRead + AsyncWrite + Unpin,
    U: InboundConnectionUpgrade<Negotiated<C>> + OutboundConnectionUpgrade<Negotiated<C>>,
{
    peer_id: Option<PeerId>,
    #[pin]
    upgrade: EitherUpgrade<C, U>,
}

impl<C, U, M, E> Future for Multiplex<C, U>
where
    C: AsyncRead + AsyncWrite + Unpin,
    U: InboundConnectionUpgrade<Negotiated<C>, Output = M, Error = E>,
    U: OutboundConnectionUpgrade<Negotiated<C>, Output = M, Error = E>,
{
    type Output = Result<(PeerId, M), UpgradeError<E>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let m = match ready!(Future::poll(this.upgrade, cx)) {
            Ok(m) => m,
            Err(err) => return Poll::Ready(Err(err)),
        };
        let i = this
            .peer_id
            .take()
            .expect("Multiplex future polled after completion.");
        Poll::Ready(Ok((i, m)))
    }
}

/// A transport with peer authentication, obtained from [`Builder::authenticate`].
#[derive(Clone)]
pub struct Authenticated<T>(Builder<T>);

impl<T> Authenticated<T>
where
    T: Transport,
    T::Error: 'static,
{
    /// Applies an arbitrary upgrade.
    ///
    /// The upgrade receives the I/O resource (i.e. connection) `C` and
    /// must produce a new I/O resource `D`. Any number of such upgrades
    /// can be performed.
    ///
    /// ## Transitions
    ///
    ///   * I/O upgrade: `C -> D`.
    ///   * Transport output: `(PeerId, C) -> (PeerId, D)`.
    pub fn apply<C, D, U, E>(self, upgrade: U) -> Authenticated<Upgrade<T, U>>
    where
        T: Transport<Output = (PeerId, C)>,
        C: AsyncRead + AsyncWrite + Unpin,
        D: AsyncRead + AsyncWrite + Unpin,
        U: InboundConnectionUpgrade<Negotiated<C>, Output = D, Error = E>,
        U: OutboundConnectionUpgrade<Negotiated<C>, Output = D, Error = E> + Clone,
        E: Error + 'static,
    {
        Authenticated(Builder::new(
            Upgrade::new(self.0.inner, upgrade),
            self.0.version,
        ))
    }

    /// Upgrades the transport with a (sub)stream multiplexer.
    ///
    /// The supplied upgrade receives the I/O resource `C` and must
    /// produce a [`StreamMuxer`] `M`. The transport must already be authenticated.
    /// This ends the (regular) transport upgrade process.
    ///
    /// ## Transitions
    ///
    ///   * I/O upgrade: `C -> M`.
    ///   * Transport output: `(PeerId, C) -> (PeerId, M)`.
    pub fn multiplex<C, M, U, E>(
        self,
        upgrade: U,
    ) -> Multiplexed<AndThen<T, impl FnOnce((PeerId, C), ConnectedPoint) -> Multiplex<C, U> + Clone>>
    where
        T: Transport<Output = (PeerId, C)>,
        C: AsyncRead + AsyncWrite + Unpin,
        M: StreamMuxer,
        U: InboundConnectionUpgrade<Negotiated<C>, Output = M, Error = E>,
        U: OutboundConnectionUpgrade<Negotiated<C>, Output = M, Error = E> + Clone,
        E: Error + 'static,
    {
        let version = self.0.version;
        Multiplexed(self.0.inner.and_then(move |(i, c), endpoint| {
            let upgrade = upgrade::apply(c, upgrade, endpoint, version);
            Multiplex {
                peer_id: Some(i),
                upgrade,
            }
        }))
    }

    /// Like [`Authenticated::multiplex`] but accepts a function which returns the upgrade.
    ///
    /// The supplied function is applied to [`PeerId`] and [`ConnectedPoint`]
    /// and returns an upgrade which receives the I/O resource `C` and must
    /// produce a [`StreamMuxer`] `M`. The transport must already be authenticated.
    /// This ends the (regular) transport upgrade process.
    ///
    /// ## Transitions
    ///
    ///   * I/O upgrade: `C -> M`.
    ///   * Transport output: `(PeerId, C) -> (PeerId, M)`.
    pub fn multiplex_ext<C, M, U, E, F>(
        self,
        up: F,
    ) -> Multiplexed<AndThen<T, impl FnOnce((PeerId, C), ConnectedPoint) -> Multiplex<C, U> + Clone>>
    where
        T: Transport<Output = (PeerId, C)>,
        C: AsyncRead + AsyncWrite + Unpin,
        M: StreamMuxer,
        U: InboundConnectionUpgrade<Negotiated<C>, Output = M, Error = E>,
        U: OutboundConnectionUpgrade<Negotiated<C>, Output = M, Error = E> + Clone,
        E: Error + 'static,
        F: for<'a> FnOnce(&'a PeerId, &'a ConnectedPoint) -> U + Clone,
    {
        let version = self.0.version;
        Multiplexed(self.0.inner.and_then(move |(peer_id, c), endpoint| {
            let upgrade = upgrade::apply(c, up(&peer_id, &endpoint), endpoint, version);
            Multiplex {
                peer_id: Some(peer_id),
                upgrade,
            }
        }))
    }
}

/// A authenticated and multiplexed transport, obtained from
/// [`Authenticated::multiplex`].
#[derive(Clone)]
#[pin_project::pin_project]
pub struct Multiplexed<T>(#[pin] T);

impl<T> Multiplexed<T> {
    /// Boxes the authenticated, multiplexed transport, including
    /// the [`StreamMuxer`] and custom transport errors.
    pub fn boxed<M>(self) -> super::Boxed<(PeerId, StreamMuxerBox)>
    where
        T: Transport<Output = (PeerId, M)> + Sized + Send + Unpin + 'static,
        T::Dial: Send + 'static,
        T::ListenerUpgrade: Send + 'static,
        T::Error: Send + Sync,
        M: StreamMuxer + Send + 'static,
        M::Substream: Send + 'static,
        M::Error: Send + Sync + 'static,
    {
        boxed(self.map(|(i, m), _| (i, StreamMuxerBox::new(m))))
    }

    /// Adds a timeout to the setup and protocol upgrade process for all
    /// inbound and outbound connections established through the transport.
    pub fn timeout(self, timeout: Duration) -> Multiplexed<TransportTimeout<T>> {
        Multiplexed(TransportTimeout::new(self.0, timeout))
    }

    /// Adds a timeout to the setup and protocol upgrade process for all
    /// outbound connections established through the transport.
    pub fn outbound_timeout(self, timeout: Duration) -> Multiplexed<TransportTimeout<T>> {
        Multiplexed(TransportTimeout::with_outgoing_timeout(self.0, timeout))
    }

    /// Adds a timeout to the setup and protocol upgrade process for all
    /// inbound connections established through the transport.
    pub fn inbound_timeout(self, timeout: Duration) -> Multiplexed<TransportTimeout<T>> {
        Multiplexed(TransportTimeout::with_ingoing_timeout(self.0, timeout))
    }
}

impl<T> Transport for Multiplexed<T>
where
    T: Transport,
{
    type Output = T::Output;
    type Error = T::Error;
    type ListenerUpgrade = T::ListenerUpgrade;
    type Dial = T::Dial;

    fn dial(
        &mut self,
        addr: Multiaddr,
        opts: DialOpts,
    ) -> Result<Self::Dial, TransportError<Self::Error>> {
        self.0.dial(addr, opts)
    }

    fn remove_listener(&mut self, id: ListenerId) -> bool {
        self.0.remove_listener(id)
    }

    fn listen_on(
        &mut self,
        id: ListenerId,
        addr: Multiaddr,
    ) -> Result<(), TransportError<Self::Error>> {
        self.0.listen_on(id, addr)
    }

    fn poll(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<TransportEvent<Self::ListenerUpgrade, Self::Error>> {
        self.project().0.poll(cx)
    }
}

/// An inbound or outbound upgrade.
type EitherUpgrade<C, U> = future::Either<InboundUpgradeApply<C, U>, OutboundUpgradeApply<C, U>>;

/// A custom upgrade on an [`Authenticated`] transport.
///
/// See [`Transport::upgrade`]
#[derive(Debug, Copy, Clone)]
#[pin_project::pin_project]
pub struct Upgrade<T, U> {
    #[pin]
    inner: T,
    upgrade: U,
}

impl<T, U> Upgrade<T, U> {
    pub fn new(inner: T, upgrade: U) -> Self {
        Upgrade { inner, upgrade }
    }
}

impl<T, C, D, U, E> Transport for Upgrade<T, U>
where
    T: Transport<Output = (PeerId, C)>,
    T::Error: 'static,
    C: AsyncRead + AsyncWrite + Unpin,
    U: InboundConnectionUpgrade<Negotiated<C>, Output = D, Error = E>,
    U: OutboundConnectionUpgrade<Negotiated<C>, Output = D, Error = E> + Clone,
    E: Error + 'static,
{
    type Output = (PeerId, D);
    type Error = TransportUpgradeError<T::Error, E>;
    type ListenerUpgrade = ListenerUpgradeFuture<T::ListenerUpgrade, U, C>;
    type Dial = DialUpgradeFuture<T::Dial, U, C>;

    fn dial(
        &mut self,
        addr: Multiaddr,
        opts: DialOpts,
    ) -> Result<Self::Dial, TransportError<Self::Error>> {
        let future = self
            .inner
            .dial(addr, opts)
            .map_err(|err| err.map(TransportUpgradeError::Transport))?;
        Ok(DialUpgradeFuture {
            future: Box::pin(future),
            upgrade: future::Either::Left(Some(self.upgrade.clone())),
        })
    }

    fn remove_listener(&mut self, id: ListenerId) -> bool {
        self.inner.remove_listener(id)
    }

    fn listen_on(
        &mut self,
        id: ListenerId,
        addr: Multiaddr,
    ) -> Result<(), TransportError<Self::Error>> {
        self.inner
            .listen_on(id, addr)
            .map_err(|err| err.map(TransportUpgradeError::Transport))
    }

    fn poll(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<TransportEvent<Self::ListenerUpgrade, Self::Error>> {
        let this = self.project();
        let upgrade = this.upgrade.clone();
        this.inner.poll(cx).map(|event| {
            event
                .map_upgrade(move |future| ListenerUpgradeFuture {
                    future: Box::pin(future),
                    upgrade: future::Either::Left(Some(upgrade)),
                })
                .map_err(TransportUpgradeError::Transport)
        })
    }
}

/// Errors produced by a transport upgrade.
#[derive(Debug)]
pub enum TransportUpgradeError<T, U> {
    /// Error in the transport.
    Transport(T),
    /// Error while upgrading to a protocol.
    Upgrade(UpgradeError<U>),
}

impl<T, U> fmt::Display for TransportUpgradeError<T, U>
where
    T: fmt::Display,
    U: fmt::Display,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TransportUpgradeError::Transport(e) => write!(f, "Transport error: {e}"),
            TransportUpgradeError::Upgrade(e) => write!(f, "Upgrade error: {e}"),
        }
    }
}

impl<T, U> Error for TransportUpgradeError<T, U>
where
    T: Error + 'static,
    U: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            TransportUpgradeError::Transport(e) => Some(e),
            TransportUpgradeError::Upgrade(e) => Some(e),
        }
    }
}

/// The [`Transport::Dial`] future of an [`Upgrade`]d transport.
pub struct DialUpgradeFuture<F, U, C>
where
    U: OutboundConnectionUpgrade<Negotiated<C>>,
    C: AsyncRead + AsyncWrite + Unpin,
{
    future: Pin<Box<F>>,
    upgrade: future::Either<Option<U>, (PeerId, OutboundUpgradeApply<C, U>)>,
}

impl<F, U, C, D> Future for DialUpgradeFuture<F, U, C>
where
    F: TryFuture<Ok = (PeerId, C)>,
    C: AsyncRead + AsyncWrite + Unpin,
    U: OutboundConnectionUpgrade<Negotiated<C>, Output = D>,
    U::Error: Error,
{
    type Output = Result<(PeerId, D), TransportUpgradeError<F::Error, U::Error>>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // We use a `this` variable because the compiler can't mutably borrow multiple times
        // across a `Deref`.
        let this = &mut *self;

        loop {
            this.upgrade = match this.upgrade {
                future::Either::Left(ref mut up) => {
                    let (i, c) = match ready!(TryFuture::try_poll(this.future.as_mut(), cx)
                        .map_err(TransportUpgradeError::Transport))
                    {
                        Ok(v) => v,
                        Err(err) => return Poll::Ready(Err(err)),
                    };
                    let u = up
                        .take()
                        .expect("DialUpgradeFuture is constructed with Either::Left(Some).");
                    future::Either::Right((i, apply_outbound(c, u, upgrade::Version::V1)))
                }
                future::Either::Right((i, ref mut up)) => {
                    let d = match ready!(
                        Future::poll(Pin::new(up), cx).map_err(TransportUpgradeError::Upgrade)
                    ) {
                        Ok(d) => d,
                        Err(err) => return Poll::Ready(Err(err)),
                    };
                    return Poll::Ready(Ok((i, d)));
                }
            }
        }
    }
}

impl<F, U, C> Unpin for DialUpgradeFuture<F, U, C>
where
    U: OutboundConnectionUpgrade<Negotiated<C>>,
    C: AsyncRead + AsyncWrite + Unpin,
{
}

/// The [`Transport::ListenerUpgrade`] future of an [`Upgrade`]d transport.
pub struct ListenerUpgradeFuture<F, U, C>
where
    C: AsyncRead + AsyncWrite + Unpin,
    U: InboundConnectionUpgrade<Negotiated<C>>,
{
    future: Pin<Box<F>>,
    upgrade: future::Either<Option<U>, (PeerId, InboundUpgradeApply<C, U>)>,
}

impl<F, U, C, D> Future for ListenerUpgradeFuture<F, U, C>
where
    F: TryFuture<Ok = (PeerId, C)>,
    C: AsyncRead + AsyncWrite + Unpin,
    U: InboundConnectionUpgrade<Negotiated<C>, Output = D>,
    U::Error: Error,
{
    type Output = Result<(PeerId, D), TransportUpgradeError<F::Error, U::Error>>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // We use a `this` variable because the compiler can't mutably borrow multiple times
        // across a `Deref`.
        let this = &mut *self;

        loop {
            this.upgrade = match this.upgrade {
                future::Either::Left(ref mut up) => {
                    let (i, c) = match ready!(TryFuture::try_poll(this.future.as_mut(), cx)
                        .map_err(TransportUpgradeError::Transport))
                    {
                        Ok(v) => v,
                        Err(err) => return Poll::Ready(Err(err)),
                    };
                    let u = up
                        .take()
                        .expect("ListenerUpgradeFuture is constructed with Either::Left(Some).");
                    future::Either::Right((i, apply_inbound(c, u)))
                }
                future::Either::Right((i, ref mut up)) => {
                    let d = match ready!(TryFuture::try_poll(Pin::new(up), cx)
                        .map_err(TransportUpgradeError::Upgrade))
                    {
                        Ok(v) => v,
                        Err(err) => return Poll::Ready(Err(err)),
                    };
                    return Poll::Ready(Ok((i, d)));
                }
            }
        }
    }
}

impl<F, U, C> Unpin for ListenerUpgradeFuture<F, U, C>
where
    C: AsyncRead + AsyncWrite + Unpin,
    U: InboundConnectionUpgrade<Negotiated<C>>,
{
}
