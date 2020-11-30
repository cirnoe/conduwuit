use crate::{
    client_server, database::rooms::ClosestParent, utils, ConduitResult, Database, Error, PduEvent,
    Result, Ruma,
};
use get_profile_information::v1::ProfileField;
use http::header::{HeaderValue, AUTHORIZATION, HOST};
use log::{error, warn};
use rocket::{get, post, put, response::content::Json, State};
use ruma::{
    api::{
        federation::{
            directory::{get_public_rooms, get_public_rooms_filtered},
            discovery::{
                get_server_keys, get_server_version::v1 as get_server_version, ServerKey, VerifyKey,
            },
            event::get_missing_events,
            query::get_profile_information,
            transactions::send_transaction_message,
        },
        OutgoingRequest,
    },
    directory::{IncomingFilter, IncomingRoomNetwork},
    serde::{to_canonical_value, CanonicalJsonObject},
    EventId, RoomId, RoomVersionId, ServerName, UserId,
};
use std::{
    collections::BTreeMap,
    convert::{TryFrom, TryInto},
    fmt::Debug,
    time::{Duration, SystemTime},
};
use trust_dns_resolver::AsyncResolver;

pub async fn request_well_known(
    globals: &crate::database::globals::Globals,
    destination: &str,
) -> Option<String> {
    let body: serde_json::Value = serde_json::from_str(
        &globals
            .reqwest_client()
            .get(&format!(
                "https://{}/.well-known/matrix/server",
                destination
            ))
            .send()
            .await
            .ok()?
            .text()
            .await
            .ok()?,
    )
    .ok()?;
    Some(body.get("m.server")?.as_str()?.to_owned())
}

pub async fn send_request<T: OutgoingRequest>(
    globals: &crate::database::globals::Globals,
    destination: Box<ServerName>,
    request: T,
) -> Result<T::IncomingResponse>
where
    T: Debug,
{
    if !globals.federation_enabled() {
        return Err(Error::bad_config("Federation is disabled."));
    }

    let resolver = AsyncResolver::tokio_from_system_conf().await.map_err(|_| {
        Error::bad_config("Failed to set up trust dns resolver with system config.")
    })?;

    let mut host = None;

    let actual_destination = "https://".to_owned()
        + &if let Some(mut delegated_hostname) =
            request_well_known(globals, &destination.as_str()).await
        {
            if let Ok(Some(srv)) = resolver
                .srv_lookup(format!("_matrix._tcp.{}", delegated_hostname))
                .await
                .map(|srv| srv.iter().next().map(|result| result.target().to_string()))
            {
                host = Some(delegated_hostname);
                srv.trim_end_matches('.').to_owned()
            } else {
                if delegated_hostname.find(':').is_none() {
                    delegated_hostname += ":8448";
                }
                delegated_hostname
            }
        } else {
            let mut destination = destination.as_str().to_owned();
            if destination.find(':').is_none() {
                destination += ":8448";
            }
            destination
        };

    let mut http_request = request
        .try_into_http_request(&actual_destination, Some(""))
        .map_err(|e| {
            warn!("Failed to find destination {}: {}", actual_destination, e);
            Error::BadServerResponse("Invalid destination")
        })?;

    let mut request_map = serde_json::Map::new();

    if !http_request.body().is_empty() {
        request_map.insert(
            "content".to_owned(),
            serde_json::from_slice(http_request.body())
                .expect("body is valid json, we just created it"),
        );
    };

    request_map.insert("method".to_owned(), T::METADATA.method.to_string().into());
    request_map.insert(
        "uri".to_owned(),
        http_request
            .uri()
            .path_and_query()
            .expect("all requests have a path")
            .to_string()
            .into(),
    );
    request_map.insert("origin".to_owned(), globals.server_name().as_str().into());
    request_map.insert("destination".to_owned(), destination.as_str().into());

    let mut request_json =
        serde_json::from_value(request_map.into()).expect("valid JSON is valid BTreeMap");

    ruma::signatures::sign_json(
        globals.server_name().as_str(),
        globals.keypair(),
        &mut request_json,
    )
    .expect("our request json is what ruma expects");

    let request_json: serde_json::Map<String, serde_json::Value> =
        serde_json::from_slice(&serde_json::to_vec(&request_json).unwrap()).unwrap();

    let signatures = request_json["signatures"]
        .as_object()
        .unwrap()
        .values()
        .map(|v| {
            v.as_object()
                .unwrap()
                .iter()
                .map(|(k, v)| (k, v.as_str().unwrap()))
        });

    for signature_server in signatures {
        for s in signature_server {
            http_request.headers_mut().insert(
                AUTHORIZATION,
                HeaderValue::from_str(&format!(
                    "X-Matrix origin={},key=\"{}\",sig=\"{}\"",
                    globals.server_name(),
                    s.0,
                    s.1
                ))
                .unwrap(),
            );
        }
    }

    if let Some(host) = host {
        http_request
            .headers_mut()
            .insert(HOST, HeaderValue::from_str(&host).unwrap());
    }

    let mut reqwest_request = reqwest::Request::try_from(http_request)
        .expect("all http requests are valid reqwest requests");

    *reqwest_request.timeout_mut() = Some(Duration::from_secs(30));

    let url = reqwest_request.url().clone();
    let reqwest_response = globals.reqwest_client().execute(reqwest_request).await;

    // Because reqwest::Response -> http::Response is complicated:
    match reqwest_response {
        Ok(mut reqwest_response) => {
            let status = reqwest_response.status();
            let mut http_response = http::Response::builder().status(status);
            let headers = http_response.headers_mut().unwrap();

            for (k, v) in reqwest_response.headers_mut().drain() {
                if let Some(key) = k {
                    headers.insert(key, v);
                }
            }

            let body = reqwest_response
                .bytes()
                .await
                .unwrap_or_else(|e| {
                    warn!("server error: {}", e);
                    Vec::new().into()
                }) // TODO: handle timeout
                .into_iter()
                .collect();

            let response = T::IncomingResponse::try_from(
                http_response
                    .body(body)
                    .expect("reqwest body is valid http body"),
            );
            response.map_err(|e| {
                warn!(
                    "Server returned bad response {} ({}): {:?}",
                    destination, url, e
                );
                Error::BadServerResponse("Server returned bad response.")
            })
        }
        Err(e) => Err(e.into()),
    }
}

#[cfg_attr(feature = "conduit_bin", get("/_matrix/federation/v1/version"))]
pub fn get_server_version(db: State<'_, Database>) -> ConduitResult<get_server_version::Response> {
    if !db.globals.federation_enabled() {
        return Err(Error::bad_config("Federation is disabled."));
    }

    Ok(get_server_version::Response {
        server: Some(get_server_version::Server {
            name: Some("Conduit".to_owned()),
            version: Some(env!("CARGO_PKG_VERSION").to_owned()),
        }),
    }
    .into())
}

#[cfg_attr(feature = "conduit_bin", get("/_matrix/key/v2/server"))]
pub fn get_server_keys(db: State<'_, Database>) -> Json<String> {
    if !db.globals.federation_enabled() {
        // TODO: Use proper types
        return Json("Federation is disabled.".to_owned());
    }

    let mut verify_keys = BTreeMap::new();
    verify_keys.insert(
        format!("ed25519:{}", db.globals.keypair().version())
            .try_into()
            .expect("DB stores valid ServerKeyId's"),
        VerifyKey {
            key: base64::encode_config(db.globals.keypair().public_key(), base64::STANDARD_NO_PAD),
        },
    );
    let mut response = serde_json::from_slice(
        http::Response::try_from(get_server_keys::v2::Response {
            server_key: ServerKey {
                server_name: db.globals.server_name().to_owned(),
                verify_keys,
                old_verify_keys: BTreeMap::new(),
                signatures: BTreeMap::new(),
                valid_until_ts: SystemTime::now() + Duration::from_secs(60 * 2),
            },
        })
        .unwrap()
        .body(),
    )
    .unwrap();

    ruma::signatures::sign_json(
        db.globals.server_name().as_str(),
        db.globals.keypair(),
        &mut response,
    )
    .unwrap();

    Json(ruma::serde::to_canonical_json_string(&response).expect("JSON is canonical"))
}

#[cfg_attr(feature = "conduit_bin", get("/_matrix/key/v2/server/<_>"))]
pub fn get_server_keys_deprecated(db: State<'_, Database>) -> Json<String> {
    get_server_keys(db)
}

#[cfg_attr(
    feature = "conduit_bin",
    post("/_matrix/federation/v1/publicRooms", data = "<body>")
)]
pub async fn get_public_rooms_filtered_route(
    db: State<'_, Database>,
    body: Ruma<get_public_rooms_filtered::v1::Request<'_>>,
) -> ConduitResult<get_public_rooms_filtered::v1::Response> {
    if !db.globals.federation_enabled() {
        return Err(Error::bad_config("Federation is disabled."));
    }

    let response = client_server::get_public_rooms_filtered_helper(
        &db,
        None,
        body.limit,
        body.since.as_deref(),
        &body.filter,
        &body.room_network,
    )
    .await?
    .0;

    Ok(get_public_rooms_filtered::v1::Response {
        chunk: response
            .chunk
            .into_iter()
            .map(|c| {
                // Convert ruma::api::federation::directory::get_public_rooms::v1::PublicRoomsChunk
                // to ruma::api::client::r0::directory::PublicRoomsChunk
                Ok::<_, Error>(
                    serde_json::from_str(
                        &serde_json::to_string(&c)
                            .expect("PublicRoomsChunk::to_string always works"),
                    )
                    .expect("federation and client-server PublicRoomsChunk are the same type"),
                )
            })
            .filter_map(|r| r.ok())
            .collect(),
        prev_batch: response.prev_batch,
        next_batch: response.next_batch,
        total_room_count_estimate: response.total_room_count_estimate,
    }
    .into())
}

#[cfg_attr(
    feature = "conduit_bin",
    get("/_matrix/federation/v1/publicRooms", data = "<body>")
)]
pub async fn get_public_rooms_route(
    db: State<'_, Database>,
    body: Ruma<get_public_rooms::v1::Request<'_>>,
) -> ConduitResult<get_public_rooms::v1::Response> {
    if !db.globals.federation_enabled() {
        return Err(Error::bad_config("Federation is disabled."));
    }

    let response = client_server::get_public_rooms_filtered_helper(
        &db,
        None,
        body.limit,
        body.since.as_deref(),
        &IncomingFilter::default(),
        &IncomingRoomNetwork::Matrix,
    )
    .await?
    .0;

    Ok(get_public_rooms::v1::Response {
        chunk: response
            .chunk
            .into_iter()
            .map(|c| {
                // Convert ruma::api::federation::directory::get_public_rooms::v1::PublicRoomsChunk
                // to ruma::api::client::r0::directory::PublicRoomsChunk
                Ok::<_, Error>(
                    serde_json::from_str(
                        &serde_json::to_string(&c)
                            .expect("PublicRoomsChunk::to_string always works"),
                    )
                    .expect("federation and client-server PublicRoomsChunk are the same type"),
                )
            })
            .filter_map(|r| r.ok())
            .collect(),
        prev_batch: response.prev_batch,
        next_batch: response.next_batch,
        total_room_count_estimate: response.total_room_count_estimate,
    }
    .into())
}

#[cfg_attr(
    feature = "conduit_bin",
    put("/_matrix/federation/v1/send/<_>", data = "<body>")
)]
pub async fn send_transaction_message_route<'a>(
    db: State<'a, Database>,
    body: Ruma<send_transaction_message::v1::Request<'_>>,
) -> ConduitResult<send_transaction_message::v1::Response> {
    if !db.globals.federation_enabled() {
        return Err(Error::bad_config("Federation is disabled."));
    }

    for edu in &body.edus {
        match serde_json::from_str::<send_transaction_message::v1::Edu>(edu.json().get()) {
            Ok(edu) => match edu.edu_type.as_str() {
                "m.typing" => {
                    if let Some(typing) = edu.content.get("typing") {
                        if typing.as_bool().unwrap_or_default() {
                            db.rooms.edus.typing_add(
                                &UserId::try_from(edu.content["user_id"].as_str().unwrap())
                                    .unwrap(),
                                &RoomId::try_from(edu.content["room_id"].as_str().unwrap())
                                    .unwrap(),
                                3000 + utils::millis_since_unix_epoch(),
                                &db.globals,
                            )?;
                        } else {
                            db.rooms.edus.typing_remove(
                                &UserId::try_from(edu.content["user_id"].as_str().unwrap())
                                    .unwrap(),
                                &RoomId::try_from(edu.content["room_id"].as_str().unwrap())
                                    .unwrap(),
                                &db.globals,
                            )?;
                        }
                    }
                }
                "m.presence" => {}
                "m.receipt" => {}
                _ => {}
            },
            Err(err) => {
                error!("{}", err);
                continue;
            }
        }
    }

    // TODO: For RoomVersion6 we must check that Raw<..> is canonical do we anywhere?
    // SPEC:
    // Servers MUST strictly enforce the JSON format specified in the appendices.
    // This translates to a 400 M_BAD_JSON error on most endpoints, or discarding of
    // events over federation. For example, the Federation API's /send endpoint would
    // discard the event whereas the Client Server API's /send/{eventType} endpoint
    // would return a M_BAD_JSON error.
    let mut resolved_map = BTreeMap::new();
    for pdu in &body.pdus {
        let (event_id, value) = process_incoming_pdu(pdu);
        // TODO: this is an unfortunate conversion dance...
        let pdu = serde_json::from_value::<PduEvent>(serde_json::to_value(&value).expect("msg"))
            .expect("all ruma pdus are conduit pdus");
        let room_id = &pdu.room_id;

        // If we have no idea about this room skip the PDU
        if !db.rooms.exists(room_id)? {
            error!("Room does not exist on this server.");
            resolved_map.insert(event_id, Err("Room is unknown to this server".into()));
            continue;
        }

        let get_state_response = match send_request(
            &db.globals,
            body.body.origin.clone(),
            ruma::api::federation::event::get_room_state::v1::Request {
                room_id,
                event_id: &event_id,
            },
        )
        .await
        {
            Ok(res) => res,
            // We can't hard fail because there are some valid errors, just
            // keep checking PDU's
            //
            // As an example a possible error
            // {"errcode":"M_FORBIDDEN","error":"Host not in room."}
            Err(err) => {
                error!("Request failed: {}", err);
                resolved_map.insert(event_id, Err(err.to_string()));
                continue;
            }
        };

        let their_current_state = get_state_response
            .pdus
            .iter()
            .chain(get_state_response.auth_chain.iter()) // add auth events
            .map(|pdu| {
                let (event_id, json) = process_incoming_pdu(pdu);
                (
                    event_id.clone(),
                    std::sync::Arc::new(
                        // When creating a StateEvent the event_id arg will be used
                        // over any found in the json and it will not use ruma::reference_hash
                        // to generate one
                        state_res::StateEvent::from_id_canon_obj(event_id, json)
                            .expect("valid pdu json"),
                    ),
                )
            })
            .collect::<BTreeMap<_, _>>();

        if value.get("state_key").is_none() {
            if !db.rooms.is_joined(&pdu.sender, room_id)? {
                error!("Sender is not joined {}", pdu.kind);
                resolved_map.insert(event_id, Err("User is not in this room".into()));
                continue;
            }

            // If the event is older than the last event in pduid_pdu Tree then find the
            // closest ancestor we know of and insert after the known ancestor by
            // altering the known events pduid to = same roomID + same count bytes + 0x1
            // pushing a single byte every time a simple append cannot be done.
            match db
                .rooms
                .get_closest_parent(room_id, &pdu.prev_events, &their_current_state)?
            {
                Some(ClosestParent::Append) => {
                    let count = db.globals.next_count()?;
                    let mut pdu_id = room_id.as_bytes().to_vec();
                    pdu_id.push(0xff);
                    pdu_id.extend_from_slice(&count.to_be_bytes());

                    db.rooms.append_pdu(
                        &pdu,
                        &value,
                        count,
                        pdu_id.into(),
                        &db.globals,
                        &db.account_data,
                        &db.admin,
                    )?;
                }
                Some(ClosestParent::Insert(old_count)) => {
                    println!("INSERT PDU FOUND {}", old_count);

                    let count = old_count;
                    let mut pdu_id = room_id.as_bytes().to_vec();
                    pdu_id.push(0xff);
                    pdu_id.extend_from_slice(&count.to_be_bytes());
                    // Create a new count that is after old_count but before
                    // the pdu appended after
                    pdu_id.push(1);

                    db.rooms.append_pdu(
                        &pdu,
                        &value,
                        count,
                        pdu_id.into(),
                        &db.globals,
                        &db.account_data,
                        &db.admin,
                    )?;
                }
                _ => panic!("Not a sequential event or no parents found"),
            };
            resolved_map.insert(event_id, Ok::<(), String>(()));
            continue;
        }

        let our_current_state = db.rooms.room_state_full(room_id)?;
        match state_res::StateResolution::resolve(
            room_id,
            &ruma::RoomVersionId::Version6,
            &[
                our_current_state
                    .iter()
                    .map(|((ev, sk), v)| ((ev.clone(), sk.to_owned()), v.event_id.clone()))
                    .collect::<BTreeMap<_, _>>(),
                their_current_state
                    .iter()
                    .map(|(_id, v)| ((v.kind(), v.state_key()), v.event_id()))
                    .collect::<BTreeMap<_, _>>(),
            ],
            Some(
                our_current_state
                    .iter()
                    .map(|(_k, v)| (v.event_id.clone(), v.convert_for_state_res()))
                    .chain(
                        their_current_state
                            .iter()
                            .map(|(id, ev)| (id.clone(), ev.clone())),
                    )
                    .collect::<BTreeMap<_, _>>(),
            ),
            &db.rooms,
        ) {
            Ok(resolved) if resolved.values().any(|id| &event_id == id) => {
                // If the event is older than the last event in pduid_pdu Tree then find the
                // closest ancestor we know of and insert after the known ancestor by
                // altering the known events pduid to = same roomID + same count bytes + 0x1
                // pushing a single byte every time a simple append cannot be done.
                match db.rooms.get_closest_parent(
                    room_id,
                    &pdu.prev_events,
                    &their_current_state,
                )? {
                    Some(ClosestParent::Append) => {
                        let count = db.globals.next_count()?;
                        let mut pdu_id = room_id.as_bytes().to_vec();
                        pdu_id.push(0xff);
                        pdu_id.extend_from_slice(&count.to_be_bytes());

                        db.rooms.append_pdu(
                            &pdu,
                            &value,
                            count,
                            pdu_id.into(),
                            &db.globals,
                            &db.account_data,
                            &db.admin,
                        )?;
                    }
                    Some(ClosestParent::Insert(old_count)) => {
                        println!("INSERT STATE PDU FOUND {}", old_count);

                        let count = old_count;
                        let mut pdu_id = room_id.as_bytes().to_vec();
                        pdu_id.push(0xff);
                        pdu_id.extend_from_slice(&count.to_be_bytes());
                        // Create a new count that is after old_count but before
                        // the pdu appended after
                        pdu_id.push(1);

                        db.rooms.append_pdu(
                            &pdu,
                            &value,
                            count,
                            pdu_id.into(),
                            &db.globals,
                            &db.account_data,
                            &db.admin,
                        )?;
                    }
                    _ => panic!("Not a sequential event or no parents found"),
                }

                resolved_map.insert(event_id, Ok::<(), String>(()));
            }
            // If the eventId is not found in the resolved state auth has failed
            Ok(_) => {
                // TODO have state_res give the actual auth error in this case
                resolved_map.insert(
                    event_id,
                    Err("This event failed authentication, not found in resolved set".into()),
                );
            }
            Err(e) => {
                resolved_map.insert(event_id, Err(e.to_string()));
            }
        };
    }

    Ok(dbg!(send_transaction_message::v1::Response { pdus: resolved_map }).into())
}

#[cfg_attr(
    feature = "conduit_bin",
    post("/_matrix/federation/v1/get_missing_events/<_>", data = "<body>")
)]
pub fn get_missing_events_route<'a>(
    db: State<'a, Database>,
    body: Ruma<get_missing_events::v1::Request<'_>>,
) -> ConduitResult<get_missing_events::v1::Response> {
    if !db.globals.federation_enabled() {
        return Err(Error::bad_config("Federation is disabled."));
    }

    let mut queued_events = body.latest_events.clone();
    let mut events = Vec::new();

    let mut i = 0;
    while i < queued_events.len() && events.len() < u64::from(body.limit) as usize {
        if let Some(pdu) = db.rooms.get_pdu_json(&queued_events[i])? {
            if body.earliest_events.contains(
                &serde_json::from_value(
                    pdu.get("event_id")
                        .cloned()
                        .ok_or_else(|| Error::bad_database("Event in db has no event_id field."))?,
                )
                .map_err(|_| Error::bad_database("Invalid event_id field in pdu in db."))?,
            ) {
                i += 1;
                continue;
            }
            queued_events.extend_from_slice(
                &serde_json::from_value::<Vec<EventId>>(
                    pdu.get("prev_events").cloned().ok_or_else(|| {
                        Error::bad_database("Invalid prev_events field of pdu in db.")
                    })?,
                )
                .map_err(|_| Error::bad_database("Invalid prev_events content in pdu in db."))?,
            );
            events.push(serde_json::from_value(pdu).expect("Raw<..> is always valid"));
        }
        i += 1;
    }

    Ok(get_missing_events::v1::Response { events }.into())
}

#[cfg_attr(
    feature = "conduit_bin",
    get("/_matrix/federation/v1/query/profile", data = "<body>")
)]
pub fn get_profile_information_route<'a>(
    db: State<'a, Database>,
    body: Ruma<get_profile_information::v1::Request<'_>>,
) -> ConduitResult<get_profile_information::v1::Response> {
    if !db.globals.federation_enabled() {
        return Err(Error::bad_config("Federation is disabled."));
    }

    let mut displayname = None;
    let mut avatar_url = None;

    match body.field {
        Some(ProfileField::DisplayName) => displayname = db.users.displayname(&body.user_id)?,
        Some(ProfileField::AvatarUrl) => avatar_url = db.users.avatar_url(&body.user_id)?,
        None => {
            displayname = db.users.displayname(&body.user_id)?;
            avatar_url = db.users.avatar_url(&body.user_id)?;
        }
    }

    Ok(get_profile_information::v1::Response {
        displayname,
        avatar_url,
    }
    .into())
}

/*
#[cfg_attr(
    feature = "conduit_bin",
    get("/_matrix/federation/v2/invite/<_>/<_>", data = "<body>")
)]
pub fn get_user_devices_route<'a>(
    db: State<'a, Database>,
    body: Ruma<membership::v1::Request<'_>>,
) -> ConduitResult<get_profile_information::v1::Response> {
    if !db.globals.federation_enabled() {
        return Err(Error::bad_config("Federation is disabled."));
    }

    let mut displayname = None;
    let mut avatar_url = None;

    match body.field {
        Some(ProfileField::DisplayName) => displayname = db.users.displayname(&body.user_id)?,
        Some(ProfileField::AvatarUrl) => avatar_url = db.users.avatar_url(&body.user_id)?,
        None => {
            displayname = db.users.displayname(&body.user_id)?;
            avatar_url = db.users.avatar_url(&body.user_id)?;
        }
    }

    Ok(get_profile_information::v1::Response {
        displayname,
        avatar_url,
    }
    .into())
}
*/

/// Generates a correct eventId for the incoming pdu.
///
/// Returns a tuple of the new `EventId` and the PDU with the eventId inserted as a `serde_json::Value`.
fn process_incoming_pdu(pdu: &ruma::Raw<ruma::events::pdu::Pdu>) -> (EventId, CanonicalJsonObject) {
    let mut value =
        serde_json::from_str(pdu.json().get()).expect("A Raw<...> is always valid JSON");

    let event_id = EventId::try_from(&*format!(
        "${}",
        ruma::signatures::reference_hash(&value, &RoomVersionId::Version6)
            .expect("ruma can calculate reference hashes")
    ))
    .expect("ruma's reference hashes are valid event ids");

    value.insert(
        "event_id".to_owned(),
        to_canonical_value(&event_id).expect("EventId is a valid CanonicalJsonValue"),
    );

    (event_id, value)
}
