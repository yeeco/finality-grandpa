// Copyright 2018-2019 Parity Technologies (UK) Ltd
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Rounds that are not the current best round are run in the background.
//!
//! This module provides utilities for managing those rounds and producing commit
//! messages from them. Any rounds that become irrelevant are dropped.
//!
//! Create a `PastRounds` struct, and drive it to completion while:
//!   - Informing it of any new finalized block heights
//!   - Passing it any validated commits (so backgrounded rounds don't produce conflicting ones)

#[cfg(feature = "std")]
use futures::ready;
use futures::prelude::*;
use futures::stream::{self, futures_unordered::FuturesUnordered};
use futures::task;
use futures::channel::mpsc;
#[cfg(feature = "std")]
use log::{trace, debug};

use std::cmp;
use std::collections::HashMap;
use std::hash::Hash;
use std::pin::Pin;
use std::task::{Context, Poll};

use crate::{Commit, BlockNumberOps};
use super::Environment;
use super::voting_round::VotingRound;

// wraps a voting round with a new future that resolves when the round can
// be discarded from the working set.
//
// that point is when the round-estimate is finalized.
struct BackgroundRound<H, N, E: Environment<H, N>> where
	H: Hash + Clone + Eq + Ord + ::std::fmt::Debug,
	N: Copy + BlockNumberOps + ::std::fmt::Debug,
{
	inner: VotingRound<H, N, E>,
	waker: Option<task::Waker>,
	finalized_number: N,
	round_committer: Option<RoundCommitter<H, N, E>>,
}

impl<H, N, E: Environment<H, N>> BackgroundRound<H, N, E> where
	H: Hash + Clone + Eq + Ord + ::std::fmt::Debug,
	N: Copy + BlockNumberOps + ::std::fmt::Debug,
{
	fn round_number(&self) -> u64 {
		self.inner.round_number()
	}

	fn is_done(&self) -> bool {
		// no need to listen on a round anymore once the estimate is finalized.
		//
		// we map `None` to true because
		//   - rounds are not backgrounded when incomplete unless we've skipped forward
		//   - if we skipped forward we may never complete this round and we don't need
		//     to keep it forever.
		self.round_committer.is_none() && self.inner.round_state().estimate
			.map_or(true, |x| x.1 <= self.finalized_number)
	}

	fn update_finalized(&mut self, new_finalized: N) {
		self.finalized_number = cmp::max(self.finalized_number, new_finalized);

		// wake up the future to be polled if done.
		if self.is_done() {
			if let Some(ref waker) = self.waker {
				waker.wake_by_ref();
			}
		}
	}
}

enum BackgroundRoundChange<H, N, E: Environment<H, N>> where
	H: Hash + Clone + Eq + Ord + ::std::fmt::Debug,
	N: Copy + BlockNumberOps + ::std::fmt::Debug,
{
	/// Background round has become irrelevant and can be discarded.
	Irrelevant(u64),
	/// Background round has a commit message to issue but should continue
	/// being driven afterwards.
	Committed(Commit<H, N, E::Signature, E::Id>),
}

impl<H, N, E: Environment<H, N>> Future for BackgroundRound<H, N, E> where
	H: Hash + Clone + Eq + Ord + ::std::fmt::Debug,
	N: Copy + BlockNumberOps + ::std::fmt::Debug,
{
	type Output = Result<BackgroundRoundChange<H, N, E>, E::Error>;

	fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
		self.waker = Some(cx.waker().clone());

		self.inner.poll(cx)?;

		self.round_committer = match self.round_committer.take() {
			None => None,
			Some(mut committer) => match committer.commit(cx, &mut self.inner)? {
				Poll::Ready(None) => None,
				Poll::Ready(Some(commit)) => return Poll::Ready(Ok(
					BackgroundRoundChange::Committed(commit)
				)),
				Poll::Pending => Some(committer),
			}
		};

		if self.is_done() {
			// if this is fully done (has committed _and_ estimate finalized)
			// we bail for real.
			Poll::Ready(Ok(BackgroundRoundChange::Irrelevant(self.round_number())))
		} else {
			Poll::Pending
		}
	}
}

impl<H, N, E: Environment<H, N>> Unpin for BackgroundRound<H, N, E> where
	H: Hash + Clone + Eq + Ord + ::std::fmt::Debug,
	N: Copy + BlockNumberOps + ::std::fmt::Debug,
{
}

struct RoundCommitter<H, N, E: Environment<H, N>> where
	H: Hash + Clone + Eq + Ord + ::std::fmt::Debug,
	N: Copy + BlockNumberOps + ::std::fmt::Debug,
{
	commit_timer: E::Timer,
	import_commits: stream::Fuse<mpsc::UnboundedReceiver<Commit<H, N, E::Signature, E::Id>>>,
	last_commit: Option<Commit<H, N, E::Signature, E::Id>>,
}

impl<H, N, E: Environment<H, N>> RoundCommitter<H, N, E> where
	H: Hash + Clone + Eq + Ord + ::std::fmt::Debug,
	N: Copy + BlockNumberOps + ::std::fmt::Debug,
{
	fn new(
		commit_timer: E::Timer,
		commit_receiver: mpsc::UnboundedReceiver<Commit<H, N, E::Signature, E::Id>>,
	) -> Self {
		RoundCommitter {
			commit_timer,
			import_commits: commit_receiver.fuse(),
			last_commit: None,
		}
	}

	fn import_commit(
		&mut self,
		voting_round: &mut VotingRound<H, N, E>,
		commit: Commit<H, N, E::Signature, E::Id>,
	) -> Result<bool, E::Error> {
		// ignore commits for a block lower than we already finalized
		if commit.target_number < voting_round.finalized().map_or_else(N::zero, |(_, n)| *n) {
			return Ok(true);
		}

		if voting_round.check_and_import_from_commit(&commit)?.is_none() {
			return Ok(false)
		}

		self.last_commit = Some(commit);

		Ok(true)
	}

	fn commit(&mut self, cx: &mut Context, voting_round: &mut VotingRound<H, N, E>)
		-> Poll<Result<Option<Commit<H, N, E::Signature, E::Id>>, E::Error>>
	{
		while let Poll::Ready(Some(commit)) = Stream::poll_next(Pin::new(&mut self.import_commits), cx) {
			if !self.import_commit(voting_round, commit)? {
				trace!(target: "afg", "Ignoring invalid commit");
			}
		}

		ready!(Future::poll(Pin::new(&mut self.commit_timer), cx))?;

		match (self.last_commit.take(), voting_round.finalized()) {
			(None, Some(_)) => {
				Poll::Ready(Ok(voting_round.finalizing_commit().cloned()))
			},
			(Some(Commit { target_number, .. }), Some((_, finalized_number))) if target_number < *finalized_number => {
				Poll::Ready(Ok(voting_round.finalizing_commit().cloned()))
			},
			_ => {
				Poll::Ready(Ok(None))
			},
		}
	}
}

struct SelfReturningFuture<F> {
	inner: Option<F>,
}

impl<F> From<F> for SelfReturningFuture<F> {
	fn from(f: F) -> Self {
		SelfReturningFuture { inner: Some(f) }
	}
}

impl<F> SelfReturningFuture<F> {
	fn mutate<X: FnOnce(&mut F)>(&mut self, x: X) {
		if let Some(ref mut inner) = self.inner {
			x(inner)
		}
	}
}

impl<F: Future + Unpin> Future for SelfReturningFuture<F> {
	type Output = (F::Output, F);

	fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
		match self.inner.take() {
			None => panic!("poll after return is not done in this module; qed"),
			Some(mut f) => match Future::poll(Pin::new(&mut f), cx) {
				Poll::Ready(item) => Poll::Ready((item, f)),
				Poll::Pending => {
					self.inner = Some(f);
					Poll::Pending
				}
			}
		}
	}
}

impl<F> Unpin for SelfReturningFuture<F> {
}

/// A stream for past rounds, which produces any commit messages from those
/// rounds and drives them to completion.
pub(super) struct PastRounds<H, N, E: Environment<H, N>> where
	H: Hash + Clone + Eq + Ord + ::std::fmt::Debug,
	N: Copy + BlockNumberOps + ::std::fmt::Debug,
{
	past_rounds: FuturesUnordered<SelfReturningFuture<BackgroundRound<H, N, E>>>,
	commit_senders: HashMap<u64, mpsc::UnboundedSender<Commit<H, N, E::Signature, E::Id>>>,
}

impl<H, N, E: Environment<H, N>> PastRounds<H, N, E> where
	H: Hash + Clone + Eq + Ord + ::std::fmt::Debug,
	N: Copy + BlockNumberOps + ::std::fmt::Debug,
{
	/// Create a new past rounds stream.
	pub(super) fn new() -> Self {
		PastRounds {
			past_rounds: FuturesUnordered::new(),
			commit_senders: HashMap::new(),
		}
	}

	// push an old voting round onto this stream.
	pub(super) fn push(&mut self, env: &E, round: VotingRound<H, N, E>) {
		let round_number = round.round_number();
		let (tx, rx) = mpsc::unbounded();
		let background = BackgroundRound {
			inner: round,
			waker: None,
			// https://github.com/paritytech/finality-grandpa/issues/50
			finalized_number: N::zero(),
			round_committer: Some(RoundCommitter::new(
				env.round_commit_timer(),
				rx,
			)),
		};
		self.past_rounds.push(background.into());
		self.commit_senders.insert(round_number, tx);
	}

	/// update the last finalized block. this will lead to
	/// any irrelevant background rounds being pruned.
	pub(super) fn update_finalized(&mut self, f_num: N) {
		// have the task check if it should be pruned.
		// if so, this future will be re-polled
		for bg in self.past_rounds.iter_mut() {
			bg.mutate(|f| f.update_finalized(f_num));
		}
	}

	// import the commit into the given backgrounded round. If not possible,
	// just return and process the commit.
	pub(super) fn import_commit(&self, round_number: u64, commit: Commit<H, N, E::Signature, E::Id>)
		-> Option<Commit<H, N, E::Signature, E::Id>>
	{
		if let Some(sender) = self.commit_senders.get(&round_number) {
			sender.unbounded_send(commit).map_err(|e| e.into_inner()).err()
		} else {
			Some(commit)
		}
	}
}

impl<H, N, E: Environment<H, N>> Stream for PastRounds<H, N, E> where
	H: Hash + Clone + Eq + Ord + ::std::fmt::Debug,
	N: Copy + BlockNumberOps + ::std::fmt::Debug,
{
	type Item = Result<(u64, Commit<H, N, E::Signature, E::Id>), E::Error>;

	fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
		loop {
			match Stream::poll_next(Pin::new(&mut self.past_rounds), cx) {
				Poll::Ready(Some((Ok(BackgroundRoundChange::Irrelevant(number)), _))) => {
					self.commit_senders.remove(&number);
				}
				Poll::Ready(Some((Ok(BackgroundRoundChange::Committed(commit)), round))) => {
					let number = round.round_number();

					// reschedule until irrelevant.
					self.past_rounds.push(round.into());

					debug!(
						target: "afg", "Committing: round_number = {}, \
						target_number = {:?}, target_hash = {:?}",
						number,
						commit.target_number,
						commit.target_hash,
					);

					return Poll::Ready(Some(Ok((number, commit))));
				}
				Poll::Ready(Some((Err(err), _))) => return Poll::Ready(Some(Err(err))),
				Poll::Ready(None) => return Poll::Ready(None),
				Poll::Pending => return Poll::Pending,
			}
		}
	}
}

impl<H, N, E: Environment<H, N>> Unpin for PastRounds<H, N, E> where
	H: Hash + Clone + Eq + Ord + ::std::fmt::Debug,
	N: Copy + BlockNumberOps + ::std::fmt::Debug,
{
}
