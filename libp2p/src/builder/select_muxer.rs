// Copyright 2018 Parity Technologies (UK) Ltd.
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

#![allow(unreachable_pub)]

use std::iter::{Chain, Map};

use either::Either;
use futures::future;
use libp2p_core::{
    either::EitherFuture,
    upgrade::{InboundConnectionUpgrade, OutboundConnectionUpgrade},
    UpgradeInfo,
};

#[derive(Debug, Clone)]
pub struct SelectMuxerUpgrade<A, B>(A, B);

impl<A, B> SelectMuxerUpgrade<A, B> {
    pub fn new(a: A, b: B) -> Self {
        SelectMuxerUpgrade(a, b)
    }
}

impl<A, B> UpgradeInfo for SelectMuxerUpgrade<A, B>
where
    A: UpgradeInfo,
    B: UpgradeInfo,
{
    type Info = Either<A::Info, B::Info>;
    type InfoIter = Chain<
        Map<<A::InfoIter as IntoIterator>::IntoIter, fn(A::Info) -> Self::Info>,
        Map<<B::InfoIter as IntoIterator>::IntoIter, fn(B::Info) -> Self::Info>,
    >;

    fn protocol_info(&self) -> Self::InfoIter {
        let a = self
            .0
            .protocol_info()
            .into_iter()
            .map(Either::Left as fn(A::Info) -> _);
        let b = self
            .1
            .protocol_info()
            .into_iter()
            .map(Either::Right as fn(B::Info) -> _);

        a.chain(b)
    }
}

impl<C, A, B, TA, TB, EA, EB> InboundConnectionUpgrade<C> for SelectMuxerUpgrade<A, B>
where
    A: InboundConnectionUpgrade<C, Output = TA, Error = EA>,
    B: InboundConnectionUpgrade<C, Output = TB, Error = EB>,
{
    type Output = future::Either<TA, TB>;
    type Error = Either<EA, EB>;
    type Future = EitherFuture<A::Future, B::Future>;

    fn upgrade_inbound(self, sock: C, info: Self::Info) -> Self::Future {
        match info {
            Either::Left(info) => EitherFuture::First(self.0.upgrade_inbound(sock, info)),
            Either::Right(info) => EitherFuture::Second(self.1.upgrade_inbound(sock, info)),
        }
    }
}

impl<C, A, B, TA, TB, EA, EB> OutboundConnectionUpgrade<C> for SelectMuxerUpgrade<A, B>
where
    A: OutboundConnectionUpgrade<C, Output = TA, Error = EA>,
    B: OutboundConnectionUpgrade<C, Output = TB, Error = EB>,
{
    type Output = future::Either<TA, TB>;
    type Error = Either<EA, EB>;
    type Future = EitherFuture<A::Future, B::Future>;

    fn upgrade_outbound(self, sock: C, info: Self::Info) -> Self::Future {
        match info {
            Either::Left(info) => EitherFuture::First(self.0.upgrade_outbound(sock, info)),
            Either::Right(info) => EitherFuture::Second(self.1.upgrade_outbound(sock, info)),
        }
    }
}
