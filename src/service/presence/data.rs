use std::sync::Arc;

use conduit::{
	debug_warn, utils,
	utils::{stream::TryIgnore, ReadyExt},
	Result,
};
use database::{Deserialized, Map};
use futures::Stream;
use ruma::{events::presence::PresenceEvent, presence::PresenceState, OwnedUserId, UInt, UserId};

use super::Presence;
use crate::{globals, users, Dep};

pub struct Data {
	presenceid_presence: Arc<Map>,
	userid_presenceid: Arc<Map>,
	services: Services,
}

struct Services {
	globals: Dep<globals::Service>,
	users: Dep<users::Service>,
}

impl Data {
	pub(super) fn new(args: &crate::Args<'_>) -> Self {
		let db = &args.db;
		Self {
			presenceid_presence: db["presenceid_presence"].clone(),
			userid_presenceid: db["userid_presenceid"].clone(),
			services: Services {
				globals: args.depend::<globals::Service>("globals"),
				users: args.depend::<users::Service>("users"),
			},
		}
	}

	pub async fn get_presence(&self, user_id: &UserId) -> Result<(u64, PresenceEvent)> {
		let count = self
			.userid_presenceid
			.get(user_id)
			.await
			.deserialized::<u64>()?;

		let key = presenceid_key(count, user_id);
		let bytes = self.presenceid_presence.get(&key).await?;
		let event = Presence::from_json_bytes(&bytes)?
			.to_presence_event(user_id, &self.services.users)
			.await;

		Ok((count, event))
	}

	pub(super) async fn set_presence(
		&self, user_id: &UserId, presence_state: &PresenceState, currently_active: Option<bool>,
		last_active_ago: Option<UInt>, status_msg: Option<String>,
	) -> Result<()> {
		let last_presence = self.get_presence(user_id).await;
		let state_changed = match last_presence {
			Err(_) => true,
			Ok(ref presence) => presence.1.content.presence != *presence_state,
		};

		let status_msg_changed = match last_presence {
			Err(_) => true,
			Ok(ref last_presence) => {
				let old_msg = last_presence
					.1
					.content
					.status_msg
					.clone()
					.unwrap_or_default();

				let new_msg = status_msg.clone().unwrap_or_default();

				new_msg != old_msg
			},
		};

		let now = utils::millis_since_unix_epoch();
		let last_last_active_ts = match last_presence {
			Err(_) => 0,
			Ok((_, ref presence)) => now.saturating_sub(presence.content.last_active_ago.unwrap_or_default().into()),
		};

		let last_active_ts = match last_active_ago {
			None => now,
			Some(last_active_ago) => now.saturating_sub(last_active_ago.into()),
		};

		// TODO: tighten for state flicker?
		if !status_msg_changed && !state_changed && last_active_ts < last_last_active_ts {
			debug_warn!("presence spam {user_id:?} last_active_ts:{last_active_ts:?} < {last_last_active_ts:?}",);
			return Ok(());
		}

		let status_msg = if status_msg.as_ref().is_some_and(String::is_empty) {
			None
		} else {
			status_msg
		};

		let presence = Presence::new(
			presence_state.to_owned(),
			currently_active.unwrap_or(false),
			last_active_ts,
			status_msg,
		);
		let count = self.services.globals.next_count()?;
		let key = presenceid_key(count, user_id);

		self.presenceid_presence
			.insert(&key, &presence.to_json_bytes()?);

		self.userid_presenceid
			.insert(user_id.as_bytes(), &count.to_be_bytes());

		if let Ok((last_count, _)) = last_presence {
			let key = presenceid_key(last_count, user_id);
			self.presenceid_presence.remove(&key);
		}

		Ok(())
	}

	pub(super) async fn remove_presence(&self, user_id: &UserId) {
		let Ok(count) = self
			.userid_presenceid
			.get(user_id)
			.await
			.deserialized::<u64>()
		else {
			return;
		};

		let key = presenceid_key(count, user_id);
		self.presenceid_presence.remove(&key);
		self.userid_presenceid.remove(user_id.as_bytes());
	}

	pub fn presence_since(&self, since: u64) -> impl Stream<Item = (OwnedUserId, u64, Vec<u8>)> + Send + '_ {
		self.presenceid_presence
			.raw_stream()
			.ignore_err()
			.ready_filter_map(move |(key, presence_bytes)| {
				let (count, user_id) = presenceid_parse(key).expect("invalid presenceid_parse");
				(count > since).then(|| (user_id.to_owned(), count, presence_bytes.to_vec()))
			})
	}
}

#[inline]
fn presenceid_key(count: u64, user_id: &UserId) -> Vec<u8> {
	[count.to_be_bytes().to_vec(), user_id.as_bytes().to_vec()].concat()
}

#[inline]
fn presenceid_parse(key: &[u8]) -> Result<(u64, &UserId)> {
	let (count, user_id) = key.split_at(8);
	let user_id = user_id_from_bytes(user_id)?;
	let count = utils::u64_from_u8(count);

	Ok((count, user_id))
}

/// Parses a `UserId` from bytes.
fn user_id_from_bytes(bytes: &[u8]) -> Result<&UserId> {
	let str: &str = utils::str_from_bytes(bytes)?;
	let user_id: &UserId = str.try_into()?;

	Ok(user_id)
}
