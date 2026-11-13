//! Paykit's shared record values (Paykit spec 6, 8.3; Paykit wire 5): stored messages,
//! activity records, `payment_lists` and `allowances`, with their binding rules. The ledger
//! of `allowance_ledgers` is [`crate::allowances::Ledger`]; its binding is
//! [`bind_ledger`].
//!
//! A value is JSON (Switch wire 1.1), sealed by the Switch under the collection's read scope
//! (Switch wire 6.2). Readers verify every stored message on use and work out status from
//! the verified messages and the claim, never from an unsigned member alone.
//!
//! Security: a record binds its key by its messages' own signed fields (Switch spec 8.2).
//! The reader recomputes the key with its own naming key and acts on a record only if the
//! messages it stores name that key's request, period, payee, app or Allowance, so no own
//! app can file a counterparty's message under another obligation, label or Allowance.
//! Writers may write every member, with `null` for absent; readers read `null` as absent.

use std::collections::HashSet;

use pubky_switch_core::enc::{b64, pubky_id, unb64, zbase32_encode};
use pubky_switch_core::ident::NamingKey;
use pubky_switch_core::message::{Message, MessagePlaintext, VerifiedSender};
use pubky_switch_core::ops::{Condition, RecordVersion};
use pubky_switch_core::records::AppCert;
use pubky_switch_core::sig::SignedDoc;
use pubky_switch_core::time::rfc3339;
use serde::Serialize;
use serde_json::{Map, Value};

use crate::allowances::Ledger;
use crate::amount::PaymentAmount;
use crate::bodies::{check_body, Body, PaymentRequest, ProposerRole};
use crate::collections::Collection;
use crate::derive::{natural_key, request_obligation, terms_hash};
use crate::error::{ensure, invalid, Error, Result};
use crate::grant::ExecutorGrant;
use crate::identifiers::{
    AllowanceId, AttemptId, PaymentEndpointIdentifier, PaymentEndpointPayload,
};
use crate::json::{bad, check_v1, parse_object, Obj};
use crate::kinds::{check_sender_scopes, Kind, OPAQUE_MEMBERS};
use crate::periods::{PeriodRef, Recurrence};
use crate::scopes::{self, Label};

/// How many `msg_id`s of messages a record doesn't store it keeps, the newest (Paykit wire
/// 5.3).
pub const RECENT_MSG_IDS: usize = 16;

/// The most Billing Periods `attempted_periods` lists (Paykit spec 8.2, Paykit wire 5.3).
pub const MAX_ATTEMPTED_PERIODS: usize = 16;

/// The most period details a Billing Period has (Paykit spec 8.2, Paykit wire 4.9): the
/// payee sends no more, and a period record holds them all, in `period_details` and
/// `earlier_details`.
pub const MAX_PERIOD_DETAILS: usize = 8;

/// The most entries a request record's `reassociations` lists (Paykit wire 5.3).
pub const MAX_REASSOCIATIONS: usize = 16;

/// The most conditions a Paykit `record.put` carries (Paykit wire 5.3 "Conditions per
/// operation"): a `claim_step` for each of the at most 16 listed periods, the
/// `record_version`, and the `server_time_in` of a write that adds a reassociation.
pub const MAX_RECORD_PUT_CONDITIONS: usize = MAX_ATTEMPTED_PERIODS + 2;

/// The canonical `msg_ids` (Paykit wire 5.3): the IDs of the messages the record stores,
/// then the 16 newest distinct IDs of the others it reflects (`others`, oldest first), each
/// ID once. Duplicates are dropped in order, each ID kept at its first place, before the 16
/// are taken, so repeats never push out an older distinct ID. Writers write this list, and
/// readers read a record's list as this list of what it names.
pub fn bounded_msg_ids(stored: &[String], others: &[String]) -> Vec<String> {
    let mut seen: HashSet<&str> = HashSet::new();
    let mut out: Vec<String> = Vec::new();
    for id in stored {
        if seen.insert(id) {
            out.push(id.clone());
        }
    }
    let rest: Vec<&String> = others.iter().filter(|id| seen.insert(id)).collect();
    let skip = rest.len().saturating_sub(RECENT_MSG_IDS);
    out.extend(rest.into_iter().skip(skip).cloned());
    out
}

/// The members a writer of a shared record value doesn't know, kept as it read them (Paykit
/// wire 5): in the value, and in each object of it the writer keeps, so a member a later
/// version adds survives an older writer.
///
/// A reader collects them when it reads a value; the writer puts each back into the object
/// it was read in when it writes the value again.
///
/// - Objects of the kinds that may validly sit beside an equal one carry their own
///   [`Origin`], where they were read: the current grant and the grants of the history, the
///   grant history entries and the `used` entries. Their members travel with the element in
///   memory, so removing, reordering or inserting elements moves no members between them, a
///   copy keeps its original's, and a new element has none.
/// - Any other object is found again by where it sits, below the nearest object that has an
///   identity of its own, which the record's rules make unique: a stored message by its
///   signature, a ledger entry or `totals.failed` item by its attempt, a reassociation by its
///   members. So a stored message moved to another role keeps its members.
///
/// The members of an object the writer no longer holds, such as a compacted ledger entry, are
/// dropped with it. An empty set, as a value built in code has, writes nothing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UnknownMembers {
    /// The read the members come from: the hash of the value read; `None` without members.
    read: Option<[u8; 32]>,
    held: Vec<HeldMembers>,
}

/// Where an element of a shared record value was read, so the members its writer doesn't know
/// go back into it alone ([`UnknownMembers`]): the read, by the hash of the value read, the
/// kind of element and its place among those of its kind. A writer puts an element's members
/// back only when its origin is of the read the record holds, so an element of another
/// record, of an older read of the same record, or built in code (no origin) gets none.
///
/// It isn't part of the element's value: two origins always compare equal, so two elements
/// equal in every member are equal wherever they were read.
#[derive(Clone, Copy, Debug, Default)]
pub struct Origin(Option<Place>);

/// An element's place in a read, for [`Origin`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Place {
    read: [u8; 32],
    kind: &'static str,
    index: u32,
}

impl PartialEq for Origin {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl Eq for Origin {}

/// The origins of the elements of one value as read, by JSON pointer.
#[derive(Clone, Debug, Default)]
pub(crate) struct Origins(Vec<(String, Place)>);

impl Origins {
    /// The origin of the element read at `pointer`.
    pub(crate) fn at(&self, pointer: &str) -> Origin {
        Origin(
            self.0
                .iter()
                .find(|(p, _)| p == pointer)
                .map(|(_, place)| *place),
        )
    }

    /// The place of the next element of `kind` read, after those already read.
    fn next(&mut self, read: [u8; 32], kind: &'static str, pointer: &str) -> Place {
        let before = self.0.iter().filter(|(_, p)| p.kind == kind).count();
        let place = Place {
            read,
            kind,
            index: u32::try_from(before).unwrap_or(u32::MAX),
        };
        self.0.push((pointer.to_owned(), place));
        place
    }
}

/// The unknown members of one object, and where the object sits.
#[derive(Clone, Debug, PartialEq, Eq)]
struct HeldMembers {
    /// The nearest object at or above it that is found by itself; `None` for the value
    /// itself.
    anchor: Option<Anchor>,
    /// The members from there down to the object.
    path: Vec<&'static str>,
    /// The members.
    members: Map<String, Value>,
}

/// How an object is found by itself.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Anchor {
    /// By its shape's name and its identity.
    Identity(&'static str, Value),
    /// By the kind and place its element carries, in the read the members come from.
    Origin(&'static str, u32),
    /// An element without an origin, built in code: nothing of what was read goes into it.
    New,
}

/// The members a writer knows in one object of a shared record value (Paykit wire 5).
pub(crate) struct Shape {
    /// The shape's name, which tells apart identities of different objects.
    pub(crate) name: &'static str,
    /// The members, dotted for nested ones, that name the object wherever it sits; empty for
    /// an object found by where it sits.
    pub(crate) id: &'static [&'static str],
    /// Whether its elements carry their [`Origin`] in memory: for objects an equal one may
    /// validly sit beside.
    pub(crate) carried: bool,
    /// The members it knows, each with what it holds.
    pub(crate) members: &'static [(&'static str, Member)],
}

/// What a known member holds, for [`UnknownMembers`].
pub(crate) enum Member {
    /// A value whose inside the writer keeps as a whole, or doesn't keep: a string, a number,
    /// an array of them, or an object it writes again in full.
    Leaf,
    /// An object of the shape.
    Object(&'static Shape),
    /// An array of objects of the shape, each found by its identity or its origin.
    Array(&'static Shape),
}

impl Shape {
    /// Whether the objects of an array of this shape are found by themselves.
    fn found_alone(&self) -> bool {
        self.carried || !self.id.is_empty()
    }
}

/// The identity of an object of `shape`, `None` for one found by where it sits.
fn identity(obj: &Map<String, Value>, shape: &Shape) -> Option<Value> {
    if shape.id.is_empty() {
        return None;
    }
    let parts = shape
        .id
        .iter()
        .map(|dotted| {
            let mut at: Option<&Value> = None;
            for (i, step) in dotted.split('.').enumerate() {
                at = if i == 0 {
                    obj.get(step)
                } else {
                    at.and_then(|v| v.get(step))
                };
            }
            at.cloned().unwrap_or(Value::Null)
        })
        .collect();
    Some(Value::Array(parts))
}

impl UnknownMembers {
    /// The unknown members of `value`, read as `shape`, and the origins of its carried
    /// elements, which the reader gives the elements it builds.
    pub(crate) fn read(value: &Map<String, Value>, shape: &'static Shape) -> (Self, Origins) {
        let read = serde_json::to_vec(value)
            .map(|bytes| pubky_switch_core::enc::hash(&bytes))
            .unwrap_or_default();
        let mut held = Vec::new();
        let mut origins = Origins::default();
        collect(
            value,
            shape,
            None,
            Vec::new(),
            "",
            read,
            &mut origins,
            &mut held,
        );
        (
            Self {
                // Without members, nothing is put back, so the read isn't kept.
                read: (!held.is_empty()).then_some(read),
                held,
            },
            origins,
        )
    }

    /// Whether there are none.
    pub fn is_empty(&self) -> bool {
        self.held.is_empty()
    }

    /// The value `json` a writer wrote, as `shape`, with the unknown members put back into the
    /// objects it keeps; `origins` names the origin of each carried element written, by its
    /// JSON pointer in `json`. Without unknown members, `json` as it is.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] if the value can't be written.
    pub(crate) fn write_into(
        &self,
        json: Vec<u8>,
        shape: &'static Shape,
        origins: &[(String, Origin)],
    ) -> Result<Vec<u8>> {
        if self.held.is_empty() {
            return Ok(json);
        }
        let cant = || Error::Invalid(format!("Paykit wire 5: {}: can't be written", shape.name));
        let mut value: Value = serde_json::from_slice(&json).map_err(|_| cant())?;
        let Value::Object(map) = &mut value else {
            return Err(cant());
        };
        self.apply(map, shape, None, Vec::new(), "", origins);
        serde_json::to_vec(&value).map_err(|_| cant())
    }

    fn apply(
        &self,
        obj: &mut Map<String, Value>,
        shape: &'static Shape,
        anchor: Option<Anchor>,
        path: Vec<&'static str>,
        pointer: &str,
        origins: &[(String, Origin)],
    ) {
        let (anchor, path) = if shape.carried {
            // Only an element of the read this record holds takes members back.
            let place = origins
                .iter()
                .find(|(p, _)| p == pointer)
                .and_then(|(_, o)| o.0)
                .filter(|place| Some(place.read) == self.read);
            (
                Some(place.map_or(Anchor::New, |p| Anchor::Origin(p.kind, p.index))),
                Vec::new(),
            )
        } else {
            match identity(obj, shape) {
                Some(id) => (Some(Anchor::Identity(shape.name, id)), Vec::new()),
                None => (anchor, path),
            }
        };
        if let Some(held) = self
            .held
            .iter()
            .find(|h| h.anchor == anchor && h.path == path)
        {
            for (name, value) in &held.members {
                obj.entry(name.clone()).or_insert_with(|| value.clone());
            }
        }
        for (name, member) in shape.members {
            let mut below = path.clone();
            below.push(name);
            let at = format!("{pointer}/{name}");
            match (member, obj.get_mut(*name)) {
                (Member::Object(s), Some(Value::Object(child))) => {
                    self.apply(child, s, anchor.clone(), below, &at, origins);
                }
                (Member::Array(s), Some(Value::Array(items))) if s.found_alone() => {
                    for (i, item) in items.iter_mut().enumerate() {
                        if let Value::Object(child) = item {
                            let at = format!("{at}/{i}");
                            self.apply(child, s, anchor.clone(), below.clone(), &at, origins);
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

#[allow(clippy::too_many_arguments)] // the walk's state, passed down as it recurses
fn collect(
    obj: &Map<String, Value>,
    shape: &'static Shape,
    anchor: Option<Anchor>,
    path: Vec<&'static str>,
    pointer: &str,
    read: [u8; 32],
    origins: &mut Origins,
    out: &mut Vec<HeldMembers>,
) {
    let (anchor, path) = if shape.carried {
        let place = origins.next(read, shape.name, pointer);
        (Some(Anchor::Origin(place.kind, place.index)), Vec::new())
    } else {
        match identity(obj, shape) {
            Some(id) => (Some(Anchor::Identity(shape.name, id)), Vec::new()),
            None => (anchor, path),
        }
    };
    let members: Map<String, Value> = obj
        .iter()
        .filter(|(name, _)| !shape.members.iter().any(|(known, _)| known == name))
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    if !members.is_empty() {
        out.push(HeldMembers {
            anchor: anchor.clone(),
            path: path.clone(),
            members,
        });
    }
    for (name, member) in shape.members {
        let mut below = path.clone();
        below.push(name);
        let at = format!("{pointer}/{name}");
        match (member, obj.get(*name)) {
            (Member::Object(s), Some(Value::Object(child))) => {
                collect(child, s, anchor.clone(), below, &at, read, origins, out);
            }
            (Member::Array(s), Some(Value::Array(items))) if s.found_alone() => {
                for (i, item) in items.iter().enumerate() {
                    if let Value::Object(child) = item {
                        let at = format!("{at}/{i}");
                        let below = below.clone();
                        collect(child, s, anchor.clone(), below, &at, read, origins, out);
                    }
                }
            }
            _ => {}
        }
    }
}

static SIGNED_DOC: Shape = Shape {
    name: "signed",
    id: &[],
    carried: false,
    members: &[("body", Member::Leaf), ("sig", Member::Leaf)],
};

static PLAINTEXT: Shape = Shape {
    name: "plaintext",
    id: &[],
    carried: false,
    members: &[
        ("sender_app", Member::Leaf),
        ("signature", Member::Leaf),
        ("json", Member::Leaf),
    ],
};

/// A stored message (Paykit wire 5.2), found by its signature wherever it sits.
static STORED: Shape = Shape {
    name: "stored message",
    id: &["plaintext.signature"],
    carried: false,
    members: &[
        ("sender", Member::Leaf),
        ("cert", Member::Object(&SIGNED_DOC)),
        ("plaintext", Member::Object(&PLAINTEXT)),
        ("accepted_at", Member::Leaf),
    ],
};

static BILLING_PERIOD: Shape = Shape {
    name: "billing_period",
    id: &[],
    carried: false,
    members: &[("starts_at", Member::Leaf), ("ends_at", Member::Leaf)],
};

static AMOUNT: Shape = Shape {
    name: "amount",
    id: &[],
    carried: false,
    members: &[("asset", Member::Leaf), ("value", Member::Leaf)],
};

static UNREQUESTED: Shape = Shape {
    name: "unrequested",
    id: &[],
    carried: false,
    members: &[
        ("app_id", Member::Leaf),
        ("endpoint_id", Member::Leaf),
        ("payload", Member::Leaf),
        ("source", Member::Leaf),
        ("list_version", Member::Leaf),
        ("single_use", Member::Leaf),
        ("amount", Member::Object(&AMOUNT)),
    ],
};

static REASSOCIATION: Shape = Shape {
    name: "reassociation",
    id: &["allowance_id", "from_period"],
    carried: false,
    members: &[
        ("allowance_id", Member::Leaf),
        ("from_period", Member::Leaf),
    ],
};

/// An activity record value (Paykit wire 5.3).
static ACTIVITY_SHAPE: Shape = Shape {
    name: "activity record",
    id: &[],
    carried: false,
    members: &[
        ("v", Member::Leaf),
        ("counterparty", Member::Leaf),
        ("request", Member::Object(&STORED)),
        ("response", Member::Object(&STORED)),
        ("cancellation", Member::Object(&STORED)),
        ("proof", Member::Object(&STORED)),
        ("receipt", Member::Object(&STORED)),
        ("period_details", Member::Object(&STORED)),
        ("earlier_details", Member::Array(&STORED)),
        ("request_hash", Member::Leaf),
        ("billing_period", Member::Object(&BILLING_PERIOD)),
        ("attempt_id", Member::Leaf),
        ("unrequested", Member::Object(&UNREQUESTED)),
        ("msg_ids", Member::Leaf),
        ("status", Member::Leaf),
        ("allowance_id", Member::Leaf),
        ("attempted_periods", Member::Leaf),
        ("reassociations", Member::Array(&REASSOCIATION)),
        ("manual_only", Member::Leaf),
    ],
};

static USED: Shape = Shape {
    name: "used",
    id: &[],
    carried: true,
    members: &[
        ("version", Member::Leaf),
        ("endpoint", Member::Leaf),
        ("attempt_id", Member::Leaf),
    ],
};

/// A `payment_lists` record value (Paykit wire 5.4).
static LIST_SHAPE: Shape = Shape {
    name: "payment_lists",
    id: &[],
    carried: false,
    members: &[
        ("v", Member::Leaf),
        ("payee", Member::Leaf),
        ("app_id", Member::Leaf),
        ("list", Member::Object(&STORED)),
        ("used", Member::Array(&USED)),
        ("msg_ids", Member::Leaf),
    ],
};

/// The members of an executor grant (Paykit wire 3.3).
const GRANT_MEMBERS: &[(&str, Member)] = &[
    ("app_id", Member::Leaf),
    ("cert", Member::Object(&SIGNED_DOC)),
    ("confirmed_generation", Member::Leaf),
    ("sig", Member::Leaf),
    ("terms_hash", Member::Leaf),
];

/// The current executor grant, found by the origin it carries.
static CURRENT_GRANT: Shape = Shape {
    name: "current grant",
    id: &[],
    carried: true,
    members: GRANT_MEMBERS,
};

/// A grant of the history, found by the origin it carries. Its own kind, so the places of
/// the history's grants don't depend on whether the record holds a current one.
static GRANT: Shape = Shape {
    name: "history grant",
    id: &[],
    carried: true,
    members: GRANT_MEMBERS,
};

static GRANT_ENTRY: Shape = Shape {
    name: "grant history entry",
    id: &[],
    carried: true,
    members: &[
        ("generation", Member::Leaf),
        ("version", Member::Leaf),
        ("grant", Member::Object(&GRANT)),
    ],
};

/// An `allowances` record value (Paykit wire 5.4).
static ALLOWANCE_SHAPE: Shape = Shape {
    name: "allowances",
    id: &[],
    carried: false,
    members: &[
        ("v", Member::Leaf),
        ("counterparty", Member::Leaf),
        ("allowance_id", Member::Leaf),
        ("proposal", Member::Object(&STORED)),
        ("response", Member::Object(&STORED)),
        ("end", Member::Object(&STORED)),
        ("conflicted", Member::Object(&STORED)),
        ("grant", Member::Object(&CURRENT_GRANT)),
        ("grants", Member::Array(&GRANT_ENTRY)),
        ("msg_ids", Member::Leaf),
    ],
};

/// A verified copy of one message (Paykit wire 5.2, Switch spec 8.2).
#[derive(Clone, Debug)]
pub struct StoredMessage {
    sender: [u8; 32],
    cert: AppCert,
    plaintext: MessagePlaintext,
    message: Message,
    body: Body,
    accepted_at: Option<u64>,
}

#[derive(Serialize)]
struct StoredOut {
    sender: String,
    cert: CertOut,
    plaintext: PlaintextOut,
    accepted_at: Option<String>,
}

#[derive(Serialize)]
struct CertOut {
    body: String,
    sig: String,
}

#[derive(Serialize)]
struct PlaintextOut {
    sender_app: String,
    signature: String,
    json: String,
}

impl StoredMessage {
    /// A stored message from a received one: the sender identity, its certificate as the
    /// inbox entry carried it, the signed plaintext and the acceptance time. It is checked as
    /// [`StoredMessage::read`] checks it, for `recipient`.
    ///
    /// # Errors
    ///
    /// As [`StoredMessage::read`].
    pub fn new(
        sender: [u8; 32],
        cert: &SignedDoc,
        plaintext: MessagePlaintext,
        accepted_at: Option<u64>,
        recipient: &[u8; 32],
    ) -> Result<Self> {
        let cert = scopes::verify_certificate(cert, &sender)?;
        if let Some(at) = accepted_at {
            ensure(
                !cert.expired_at(at),
                "Paykit wire 5.2: the certificate had expired at accepted_at (Switch wire 5.1 check 3)",
            )?;
        }
        ensure(
            cert.app_id() == plaintext.sender_app_id(),
            "Paykit wire 5.2: the plaintext's App ID isn't the certificate's (Switch wire 5.1 check 6)",
        )?;
        let message = plaintext.check(
            recipient,
            &sender,
            &VerifiedSender::from_cert(&cert),
            OPAQUE_MEMBERS,
        )?;
        let own_kind = Kind::from_kind(&message.kind).is_some_and(Kind::sent_between_own_apps);
        ensure(
            message.is_from_own_app() == own_kind,
            "Paykit wire 4.3: executor_notice goes between own apps, every other kind between identities",
        )?;
        // Check 8's sender side, as at the inbox: the kind and its label are the sender's
        // to send (Paykit spec 3.3, 3.4). Which own app received it isn't recorded, so the
        // recipient side stays the inbox's.
        check_sender_scopes(&message, cert.scopes())?;
        let body = check_body(&message, plaintext.json())?;
        Ok(Self {
            sender,
            cert,
            plaintext,
            message,
            body,
            accepted_at,
        })
    }

    /// A stored message from one the Switch client received and checked: the sender, its
    /// certificate, the signed plaintext and the parsed message the client gives. The stored
    /// copy is rebuilt from the sender, the certificate and the plaintext alone, through
    /// [`StoredMessage::new`], as a reader rebuilds it from the record (Paykit wire 5.2), and
    /// `message` must be what that plaintext parses to, addressed as it says. So the copy a
    /// record holds is always the one its readers verify, and its parts can't disagree.
    ///
    /// # Errors
    ///
    /// As [`StoredMessage::new`], and [`Error::Invalid`] for a `message` or certificate that
    /// isn't the plaintext's.
    pub fn from_received(
        sender: [u8; 32],
        cert: AppCert,
        plaintext: MessagePlaintext,
        message: Message,
        accepted_at: Option<u64>,
    ) -> Result<Self> {
        let stored = Self::new(sender, cert.signed(), plaintext, accepted_at, &message.to)?;
        ensure(
            stored.message == message && stored.cert.signed() == cert.signed(),
            "Paykit wire 5.2: the parsed message or certificate isn't the stored plaintext's",
        )?;
        Ok(stored)
    }

    /// Read a stored message addressed to `recipient` (Paykit wire 5.2), with the recipient
    /// checks that need no `identity.json` (Switch wire 5.1 checks 2 to 6), Paykit's
    /// certificate checks (Paykit wire 3.1), the sender's side of check 8
    /// ([`check_sender_scopes`]) and the body (check 10). The sender's standing,
    /// check 7, was checked once when the message was recorded (Switch spec 8.2).
    ///
    /// # Errors
    ///
    /// [`Error::Switch`] for the Switch's checks, [`Error::Invalid`] for Paykit's.
    pub fn read(value: &Value, recipient: &[u8; 32]) -> Result<Self> {
        const CTX: &str = "Paykit wire 5.2: stored message";
        let obj = Obj::of(value, CTX, "stored message")?;
        let cert = obj.req_obj("cert")?;
        let cert = SignedDoc {
            body: cert.req_str("body")?.to_owned(),
            sig: cert.req_str("sig")?.to_owned(),
        };
        let p = obj.req_obj("plaintext")?;
        let json = unb64(p.req_str("json")?).map_err(|_| bad(CTX, "json", "not base64url"))?;
        let plaintext = MessagePlaintext::from_parts(
            p.req_b64::<16>("sender_app")?,
            p.req_b64::<64>("signature")?,
            json,
        )?;
        Self::new(
            obj.req_pubky("sender")?,
            &cert,
            plaintext,
            obj.opt_time("accepted_at")?,
            recipient,
        )
    }

    fn out(&self) -> StoredOut {
        StoredOut {
            sender: pubky_id(&self.sender),
            cert: CertOut {
                body: self.cert.signed().body.clone(),
                sig: self.cert.signed().sig.clone(),
            },
            plaintext: PlaintextOut {
                sender_app: b64(&self.plaintext.sender_app_id()),
                signature: b64(&self.plaintext.signature()),
                json: b64(self.plaintext.json()),
            },
            accepted_at: self.accepted_at.map(rfc3339),
        }
    }

    /// The sender identity.
    pub fn sender(&self) -> [u8; 32] {
        self.sender
    }

    /// The sender's certificate, verified.
    pub fn cert(&self) -> &AppCert {
        &self.cert
    }

    /// The signed plaintext.
    pub fn plaintext(&self) -> &MessagePlaintext {
        &self.plaintext
    }

    /// The message's common fields.
    pub fn message(&self) -> &Message {
        &self.message
    }

    /// The body, read by its kind.
    pub fn body(&self) -> &Body {
        &self.body
    }

    /// The acceptance time at the recipient's inbox, when known.
    pub fn accepted_at(&self) -> Option<u64> {
        self.accepted_at
    }

    /// The message hash (Paykit wire 1.5).
    pub fn hash(&self) -> [u8; 32] {
        self.plaintext.hash()
    }
}

/// Which activity collection a record is in (Paykit spec 6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivitySide {
    /// `payer_activity`.
    Payer,
    /// `payee_activity`.
    Payee,
}

impl ActivitySide {
    /// The collection.
    pub fn collection(self) -> Collection {
        match self {
            Self::Payer => Collection::PayerActivity,
            Self::Payee => Collection::PayeeActivity,
        }
    }
}

/// Where a payment without a request's destination came from (Paykit wire 5.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DestinationSource {
    /// `request`.
    Request,
    /// `list`: a Private Payment List.
    List,
    /// `public`: the payee's public endpoints.
    Public,
}

/// The destination of a payment without a request (Paykit wire 5.3 `unrequested`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnrequestedDestination {
    /// The payee app.
    pub app_id: [u8; 16],
    /// The identifier paid.
    pub endpoint_id: PaymentEndpointIdentifier,
    /// The payload paid.
    pub payload: PaymentEndpointPayload,
    /// Where it came from.
    pub source: DestinationSource,
    /// The list's version, for a list.
    pub list_version: Option<u64>,
    /// Whether the endpoint was single-use.
    pub single_use: bool,
    /// The asset and amount being paid, written with the `attempt`; `None` in a record whose
    /// writer doesn't know the member, until the proof is stored.
    pub amount: Option<PaymentAmount>,
}

/// An entry of a payer's request record's `reassociations` (Paykit spec 8.3, Paykit wire
/// 5.3): the Allowance the user moved the request's Billing Periods to, from `from_period`
/// on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reassociation {
    /// The Allowance the periods are moved to.
    pub allowance_id: AllowanceId,
    /// The start, in Unix seconds as in `attempted_periods`, of the first Billing Period it
    /// covers.
    pub from_period: u64,
}

/// What an activity record is, by its messages' signed fields (Paykit wire 5.3 "Binding").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordBinding {
    /// A request record: its stored request's hash.
    Request {
        /// The request hash.
        request_hash: [u8; 32],
    },
    /// A Billing Period record: the request and period every message names.
    Period {
        /// The request hash.
        request_hash: [u8; 32],
        /// The Billing Period.
        billing_period: PeriodRef,
    },
    /// A payment without a request.
    Unrequested,
}

/// An activity record value of `payer_activity` or `payee_activity` (Paykit wire 5.3): one
/// message per role.
#[derive(Clone, Debug)]
pub struct ActivityRecord {
    /// Which collection.
    pub side: ActivitySide,
    /// The other identity.
    pub counterparty: [u8; 32],
    /// The `payment_request`.
    pub request: Option<StoredMessage>,
    /// The acceptance or rejection.
    pub response: Option<StoredMessage>,
    /// A cancellation from either side.
    pub cancellation: Option<StoredMessage>,
    /// The proof.
    pub proof: Option<StoredMessage>,
    /// The receipt.
    pub receipt: Option<StoredMessage>,
    /// The period details, in a period record.
    pub period_details: Option<StoredMessage>,
    /// A period record's `request_hash` member.
    pub request_hash: Option<[u8; 32]>,
    /// A period record's `billing_period` member.
    pub billing_period: Option<PeriodRef>,
    /// A payment without a request's attempt.
    pub attempt_id: Option<AttemptId>,
    /// A payment without a request's destination.
    pub unrequested: Option<UnrequestedDestination>,
    /// The `msg_id`s the record reflects.
    pub msg_ids: Vec<String>,
    /// Informational, written by the last writer, never trusted.
    pub status: Option<String>,
    /// A payer's request record: the Allowance its first automatic payment was made under.
    pub allowance_id: Option<AllowanceId>,
    /// A payer's request record of a recurring request: the start, in Unix seconds, of every
    /// Billing Period that may have an open attempt; empty when absent.
    pub attempted_periods: Vec<u64>,
    /// A period record: every other period details held for the period, in the order of
    /// Paykit wire 4.9, at most 7; empty when absent.
    pub earlier_details: Vec<StoredMessage>,
    /// A payer's request record of a recurring request, beside `allowance_id`: the Allowances
    /// the user moved later Billing Periods to, by ascending `from_period`; empty when absent.
    pub reassociations: Vec<Reassociation>,
    /// A payer's request record of a one-time request, or a payer's period record: no
    /// executor pays the occurrence automatically. Never lifted once set.
    pub manual_only: bool,
    /// The members the writer doesn't know, kept as read (Paykit wire 5).
    pub unknown: UnknownMembers,
}

const ACTIVITY: &str = "Paykit wire 5.3: activity record";

/// The stored-message members of an activity record, the kinds each holds and the side that
/// sends them (Paykit wire 4.3, 5.3).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Sends {
    Payee,
    Payer,
    Either,
}

const ROLES: [(&str, &[Kind], Sends); 6] = [
    ("request", &[Kind::PaymentRequest], Sends::Payee),
    (
        "response",
        &[
            Kind::PaymentRequestAcceptance,
            Kind::PaymentRequestRejection,
        ],
        Sends::Payer,
    ),
    (
        "cancellation",
        &[Kind::PaymentRequestCancellation],
        Sends::Either,
    ),
    ("proof", &[Kind::PaymentProof], Sends::Payer),
    ("receipt", &[Kind::PaymentReceipt], Sends::Payee),
    (
        "period_details",
        &[Kind::PaymentPeriodDetails],
        Sends::Payee,
    ),
];

/// The member of a period record that holds the other period details of its period (Paykit
/// wire 5.3), each read as `period_details` is.
const EARLIER_DETAILS: (&str, &[Kind], Sends) = (
    "earlier_details",
    &[Kind::PaymentPeriodDetails],
    Sends::Payee,
);

#[derive(Serialize)]
struct ActivityOut<'a> {
    v: u64,
    counterparty: String,
    request: Option<StoredOut>,
    response: Option<StoredOut>,
    cancellation: Option<StoredOut>,
    proof: Option<StoredOut>,
    receipt: Option<StoredOut>,
    unrequested: Option<Value>,
    attempt_id: Option<String>,
    msg_ids: &'a [String],
    status: Option<&'a str>,
    period_details: Option<StoredOut>,
    request_hash: Option<String>,
    billing_period: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    allowance_id: Option<&'a str>,
    #[serde(skip_serializing_if = "<[u64]>::is_empty")]
    attempted_periods: &'a [u64],
    #[serde(skip_serializing_if = "Vec::is_empty")]
    earlier_details: Vec<StoredOut>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    reassociations: Vec<ReassociationOut<'a>>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    manual_only: bool,
}

#[derive(Serialize)]
struct ReassociationOut<'a> {
    allowance_id: &'a str,
    from_period: u64,
}

/// Read `msg_ids` (Paykit wire 5.3), each a lower-case UUID v4, as its canonical list for a
/// record storing the messages `stored`: [`bounded_msg_ids`] of what it names. A reader
/// keeps no more than a writer would, so a record listing repeats, or more than 16 other
/// IDs, reads as the list its writer should have written.
fn read_msg_ids(obj: &Obj<'_>, stored: &[String]) -> Result<Vec<String>> {
    let mut named: Vec<String> = Vec::new();
    for id in obj.opt_arr("msg_ids")?.into_iter().flatten() {
        let id = id
            .as_str()
            .ok_or_else(|| bad(obj.ctx(), "msg_ids", "not a string"))?;
        pubky_switch_core::json::check_uuid_v4(id)
            .map_err(|_| bad(obj.ctx(), "msg_ids", "not a lower-case UUID v4"))?;
        named.push(id.to_owned());
    }
    Ok(bounded_msg_ids(stored, &named))
}

/// The `msg_ids` a writer writes (Paykit wire 5.3, 5.4): the canonical list,
/// [`bounded_msg_ids`] of the stored messages' IDs and `others`, each a lower-case UUID v4,
/// as readers read them (Switch wire 1.1).
///
/// # Errors
///
/// [`Error::Validation`] for an ID that isn't a lower-case UUID v4.
fn written_msg_ids(stored: &[String], others: &[String]) -> Result<Vec<String>> {
    let ids = bounded_msg_ids(stored, others);
    for id in &ids {
        pubky_switch_core::json::check_uuid_v4(id).map_err(|_| {
            Error::Validation(
                "Paykit wire 5.3: msg_ids: an ID that isn't a lower-case UUID v4".into(),
            )
        })?;
    }
    Ok(ids)
}

/// The `msg_id`s of stored messages, in order.
fn msg_ids_of<'a>(stored: impl IntoIterator<Item = &'a StoredMessage>) -> Vec<String> {
    stored
        .into_iter()
        .map(|s| s.message.msg_id.clone())
        .collect()
}

fn read_attempted_periods(obj: &Obj<'_>) -> Result<Vec<u64>> {
    let items = obj
        .opt_arr("attempted_periods")?
        .map_or(&[][..], Vec::as_slice);
    ensure(
        items.len() <= MAX_ATTEMPTED_PERIODS,
        "Paykit wire 5.3: attempted_periods lists at most 16 periods",
    )?;
    let out = items
        .iter()
        .map(|item| crate::json::as_int(item, obj.ctx(), "attempted_periods"))
        .collect::<Result<Vec<u64>>>()?;
    check_attempted_periods(&out)?;
    Ok(out)
}

/// The shape of `attempted_periods` (Paykit wire 5.3): at most 16, ascending, each once.
fn check_attempted_periods(periods: &[u64]) -> Result<()> {
    ensure(
        periods.len() <= MAX_ATTEMPTED_PERIODS,
        "Paykit wire 5.3: attempted_periods lists at most 16 periods",
    )?;
    ensure(
        periods.windows(2).all(|w| w.first() < w.get(1)),
        "Paykit wire 5.3: attempted_periods is ascending, each start once",
    )
}

fn read_unrequested(value: &Value) -> Result<UnrequestedDestination> {
    let u = Obj::of(value, ACTIVITY, "unrequested")?;
    let source = match u.req_str("source")? {
        "request" => DestinationSource::Request,
        "list" => DestinationSource::List,
        "public" => DestinationSource::Public,
        _ => {
            return Err(bad(
                ACTIVITY,
                "unrequested.source",
                "is request, list or public",
            ))
        }
    };
    let list_version = u.opt_int("list_version")?;
    if (source == DestinationSource::List) != list_version.is_some() {
        return Err(bad(
            ACTIVITY,
            "unrequested.list_version",
            "for a list, and only for a list",
        ));
    }
    Ok(UnrequestedDestination {
        app_id: u.req_app_id("app_id")?,
        endpoint_id: PaymentEndpointIdentifier::parse(u.req_str("endpoint_id")?)?,
        payload: PaymentEndpointPayload::parse(u.req_str("payload")?)?,
        source,
        list_version,
        single_use: u
            .opt_bool("single_use")?
            .ok_or_else(|| bad(ACTIVITY, "unrequested.single_use", "missing"))?,
        amount: u
            .opt("amount")
            .map(|a| crate::bodies::read_amount(a, ACTIVITY, "unrequested.amount"))
            .transpose()?,
    })
}

fn read_reassociations(obj: &Obj<'_>) -> Result<Vec<Reassociation>> {
    let mut out = Vec::new();
    for item in obj.opt_arr("reassociations")?.into_iter().flatten() {
        let entry = Obj::of(item, ACTIVITY, "reassociations")?;
        out.push(Reassociation {
            allowance_id: AllowanceId::parse(entry.req_str("allowance_id")?)?,
            from_period: entry.req_int("from_period")?,
        });
    }
    Ok(out)
}

/// The shape of `reassociations` (Paykit wire 5.3): at most 16, `from_period` strictly
/// ascending, the first naming another Allowance than `allowance_id`, each another than the
/// entry before it.
fn check_reassociations(
    entries: &[Reassociation],
    allowance_id: Option<&AllowanceId>,
) -> Result<()> {
    ensure(
        entries.len() <= MAX_REASSOCIATIONS,
        "Paykit wire 5.3: reassociations lists at most 16 entries",
    )?;
    ensure(
        entries
            .windows(2)
            .all(|w| matches!(w, [a, b] if a.from_period < b.from_period)),
        "Paykit wire 5.3: reassociations is strictly ascending by from_period",
    )?;
    let mut before = allowance_id;
    for entry in entries {
        ensure(
            before.is_none_or(|b| b != &entry.allowance_id),
            "Paykit wire 5.3: a reassociation names the Allowance before it, allowance_id for the first",
        )?;
        before = Some(&entry.allowance_id);
    }
    Ok(())
}

/// Whether `start`, in Unix seconds, is the start of one of `recurrence`'s Billing Periods,
/// from the first through the last, in the unit the wire names periods by: the period's
/// `start_secs`, its `starts_at` cut to whole seconds (Paykit spec 8.2, Paykit wire 1.5,
/// 4.4). A period starting at `...00.500Z` is named by its second, `...00Z`, so the period
/// starting within that second is looked up.
fn is_period_start(recurrence: &Recurrence, start: u64) -> bool {
    let Some(first) = start.checked_mul(1000) else {
        return false;
    };
    [Some(first), first.checked_add(999)]
        .into_iter()
        .flatten()
        .any(|ms| {
            recurrence
                .lookup(ms)
                .ok()
                .flatten()
                .is_some_and(|p| p.start_secs() == start)
        })
}

fn unrequested_json(u: &UnrequestedDestination) -> Value {
    let mut map = Map::new();
    map.insert("app_id".into(), Value::String(zbase32_encode(&u.app_id)));
    map.insert(
        "endpoint_id".into(),
        Value::String(u.endpoint_id.as_str().into()),
    );
    if let Some(v) = u.list_version {
        map.insert("list_version".into(), Value::from(v));
    }
    map.insert("payload".into(), Value::String(u.payload.as_str().into()));
    map.insert("single_use".into(), Value::Bool(u.single_use));
    let source = match u.source {
        DestinationSource::Request => "request",
        DestinationSource::List => "list",
        DestinationSource::Public => "public",
    };
    map.insert("source".into(), Value::String(source.into()));
    if let Some(amount) = &u.amount {
        map.insert("amount".into(), crate::bodies::amount_json(amount));
    }
    Value::Object(map)
}

/// The request hash a stored message names at its place in `refs` (Paykit wire 5.3
/// "Binding"): its own hash for the request, second in a receipt, first otherwise.
fn named_request(role: &str, stored: &StoredMessage) -> Option<[u8; 32]> {
    match role {
        "request" => Some(stored.hash()),
        "receipt" => stored.message.refs.get(1).copied(),
        _ => stored.message.refs.first().copied(),
    }
}

/// A stored message's role, and the request and Billing Period it names.
type NamedBinding<'a> = (&'a str, Option<[u8; 32]>, Option<PeriodRef>);

impl ActivityRecord {
    /// An empty record of `side` with `counterparty`, which a writer fills before it writes
    /// it.
    pub fn new(side: ActivitySide, counterparty: [u8; 32]) -> Self {
        Self {
            side,
            counterparty,
            request: None,
            response: None,
            cancellation: None,
            proof: None,
            receipt: None,
            period_details: None,
            request_hash: None,
            billing_period: None,
            attempt_id: None,
            unrequested: None,
            msg_ids: Vec::new(),
            status: None,
            allowance_id: None,
            attempted_periods: Vec::new(),
            earlier_details: Vec::new(),
            reassociations: Vec::new(),
            manual_only: false,
            unknown: UnknownMembers::default(),
        }
    }

    /// The stored messages, by role: the members of Paykit wire 5.3 in order, then each of
    /// `earlier_details`.
    pub fn stored(&self) -> Vec<(&'static str, &StoredMessage)> {
        let members = [
            ("request", &self.request),
            ("response", &self.response),
            ("cancellation", &self.cancellation),
            ("proof", &self.proof),
            ("receipt", &self.receipt),
            ("period_details", &self.period_details),
        ];
        members
            .into_iter()
            .filter_map(|(role, s)| s.as_ref().map(|s| (role, s)))
            .chain(self.earlier_details.iter().map(|s| (EARLIER_DETAILS.0, s)))
            .collect()
    }

    /// Every period details the record holds, `period_details` first, then
    /// `earlier_details`.
    pub fn details(&self) -> Vec<&StoredMessage> {
        self.period_details
            .iter()
            .chain(self.earlier_details.iter())
            .collect()
    }

    /// The `msg_id`s of the messages the record stores, by role.
    fn stored_msg_ids(&self) -> Vec<String> {
        msg_ids_of(self.stored().into_iter().map(|(_, s)| s))
    }

    fn payer_payee(&self, own: &[u8; 32]) -> ([u8; 32], [u8; 32]) {
        match self.side {
            ActivitySide::Payer => (*own, self.counterparty),
            ActivitySide::Payee => (self.counterparty, *own),
        }
    }

    /// Read an activity record value as the identity `own` holds it (Paykit wire 5.3): each
    /// stored message verified, from the side that sends its kind and addressed to the
    /// other; the binding; the `refs` that tie the messages to the request and the proof;
    /// and the members' rules.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] or [`Error::Switch`] naming the rule: the record is invalid.
    pub fn read(json: &[u8], own: &[u8; 32], side: ActivitySide) -> Result<Self> {
        let map = parse_object(json, ACTIVITY)?;
        let obj = Obj::new(&map, ACTIVITY);
        check_v1(&obj)?;
        let counterparty = obj.req_pubky("counterparty")?;
        ensure(
            &counterparty != own,
            "Paykit wire 5.3: the counterparty is another identity",
        )?;
        let (payer, payee) = match side {
            ActivitySide::Payer => (*own, counterparty),
            ActivitySide::Payee => (counterparty, *own),
        };
        let mut roles: Vec<Option<StoredMessage>> = Vec::with_capacity(ROLES.len());
        for (role, kinds, sends) in ROLES {
            let Some(value) = obj.opt(role) else {
                roles.push(None);
                continue;
            };
            let sender = Obj::of(value, ACTIVITY, role)?.req_pubky("sender")?;
            let side_ok = match sends {
                Sends::Payee => sender == payee,
                Sends::Payer => sender == payer,
                Sends::Either => sender == payer || sender == payee,
            };
            ensure(
                side_ok,
                "Paykit wire 5.3: a stored message from the wrong side",
            )?;
            let recipient = if sender == payer { payee } else { payer };
            let stored = StoredMessage::read(value, &recipient)?;
            ensure(
                kinds.contains(&stored.body.kind()),
                "Paykit wire 5.3: a member holds another kind",
            )?;
            roles.push(Some(stored));
        }
        let [request, response, cancellation, proof, receipt, period_details]: [Option<
            StoredMessage,
        >; 6] = roles
            .try_into()
            .map_err(|_| Error::Invalid(format!("{ACTIVITY}: roles")))?;
        let earlier = obj
            .opt_arr(EARLIER_DETAILS.0)?
            .map_or(&[][..], Vec::as_slice);
        let mut earlier_details = Vec::with_capacity(earlier.len());
        for value in earlier {
            let sender = Obj::of(value, ACTIVITY, EARLIER_DETAILS.0)?.req_pubky("sender")?;
            ensure(
                sender == payee,
                "Paykit wire 5.3: a stored message from the wrong side",
            )?;
            let stored = StoredMessage::read(value, &payer)?;
            ensure(
                EARLIER_DETAILS.1.contains(&stored.body.kind()),
                "Paykit wire 5.3: a member holds another kind",
            )?;
            earlier_details.push(stored);
        }
        let manual_only = match obj.opt_bool("manual_only")? {
            None => false,
            Some(true) => true,
            Some(false) => return Err(bad(ACTIVITY, "manual_only", "is true or absent")),
        };
        let record = Self {
            side,
            counterparty,
            request,
            response,
            cancellation,
            proof,
            receipt,
            period_details,
            request_hash: obj.opt_b64::<32>("request_hash")?,
            billing_period: obj
                .opt("billing_period")
                .map(|p| PeriodRef::from_json(p, ACTIVITY))
                .transpose()?,
            attempt_id: obj.opt_b64::<16>("attempt_id")?.map(AttemptId::from_bytes),
            unrequested: obj.opt("unrequested").map(read_unrequested).transpose()?,
            msg_ids: Vec::new(),
            status: obj.opt_str("status")?.map(str::to_owned),
            allowance_id: obj
                .opt_str("allowance_id")?
                .map(AllowanceId::parse)
                .transpose()?,
            attempted_periods: read_attempted_periods(&obj)?,
            earlier_details,
            reassociations: read_reassociations(&obj)?,
            manual_only,
            unknown: UnknownMembers::read(&map, &ACTIVITY_SHAPE).0,
        };
        let mut record = record;
        record.msg_ids = read_msg_ids(&obj, &record.stored_msg_ids())?;
        record.check()?;
        Ok(record)
    }

    /// The record's rules (Paykit wire 5.3), the same for the reader and the writer: the
    /// stored messages are between the counterparty and one other identity, each of its
    /// role's kind, from the side that sends it and addressed to the other; the members'
    /// shapes; the binding; the members only some records carry; the `refs` that tie the
    /// messages together; `attempt_id`, and `unrequested.amount` when present, the stored
    /// proof's; `attempted_periods`, only in a payer's request record of a recurring request,
    /// ascending, each once, at most 16; `reassociations`, there too, beside `allowance_id`,
    /// each from a Billing Period start of the request; `manual_only`, only in a payer's
    /// request record of a one-time request or a payer's period record; and the period
    /// details ([`ActivityRecord::details`]), at most 8, `period_details` the latest unless a
    /// proof names it.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] naming the rule: the record is invalid.
    fn check(&self) -> Result<()> {
        let mut own: Option<[u8; 32]> = None;
        for (role, stored) in self.stored() {
            let (kinds, sends) = ROLES
                .iter()
                .chain(std::iter::once(&EARLIER_DETAILS))
                .find(|(r, _, _)| *r == role)
                .map(|(_, k, s)| (*k, *s))
                .ok_or_else(|| Error::Invalid(format!("{ACTIVITY}: roles")))?;
            ensure(
                kinds.contains(&stored.body.kind()),
                "Paykit wire 5.3: a member holds another kind",
            )?;
            let (sender, to) = (stored.sender, stored.message.to);
            let other = if sender == self.counterparty {
                to
            } else {
                sender
            };
            ensure(
                (sender == self.counterparty || to == self.counterparty)
                    && other != self.counterparty
                    && own.is_none_or(|o| o == other),
                "Paykit wire 5.3: a stored message isn't between this identity and the counterparty",
            )?;
            own = Some(other);
            let (payer, payee) = self.payer_payee(&other);
            let side_ok = match sends {
                Sends::Payee => sender == payee,
                Sends::Payer => sender == payer,
                Sends::Either => sender == payer || sender == payee,
            };
            ensure(
                side_ok,
                "Paykit wire 5.3: a stored message from the wrong side",
            )?;
        }
        if let Some(p) = &self.billing_period {
            ensure(
                p.ends_at > p.starts_at,
                "Paykit wire 5.3: billing_period: ends_at isn't after starts_at",
            )?;
        }
        if let Some(u) = &self.unrequested {
            ensure(
                (u.source == DestinationSource::List) == u.list_version.is_some(),
                "Paykit wire 5.3: unrequested.list_version: for a list, and only for a list",
            )?;
        }
        check_attempted_periods(&self.attempted_periods)?;
        check_reassociations(&self.reassociations, self.allowance_id.as_ref())?;
        // Paykit wire 4.11: a receipt confirms the proof's attempt, of the proof's Billing
        // Period; in the record of a payment without a request, the record's attempt. Checked
        // before the binding, which would refuse a receipt of another period only as a
        // message of another period than the record's.
        let receipt = match self.receipt.as_ref().map(StoredMessage::body) {
            Some(Body::PaymentReceipt(r)) => Some(r),
            _ => None,
        };
        if let (Some(r), Some(Body::PaymentProof(p))) =
            (receipt, self.proof.as_ref().map(StoredMessage::body))
        {
            ensure(
                r.attempt_id == p.attempt_id && r.billing_period == p.billing_period,
                "Paykit wire 4.11: the receipt isn't of the stored proof's attempt and Billing Period",
            )?;
        }
        let binding = self.binding()?;
        self.check_members(&binding)?;
        self.check_details()?;
        self.check_refs(&binding)?;
        if let (RecordBinding::Unrequested, Some(a), Some(Body::PaymentProof(p))) = (
            binding,
            self.attempt_id,
            self.proof.as_ref().map(StoredMessage::body),
        ) {
            ensure(
                p.attempt_id == a,
                "Paykit wire 5.3: attempt_id isn't the stored proof's",
            )?;
        }
        // Paykit wire 4.10: in the record of a payment without a request, the proof names the
        // list version exactly when it paid a list.
        if let (RecordBinding::Unrequested, Some(u), Some(Body::PaymentProof(p))) = (
            binding,
            &self.unrequested,
            self.proof.as_ref().map(StoredMessage::body),
        ) {
            ensure(
                p.list_version == u.list_version,
                "Paykit wire 4.10: the proof's list_version isn't the destination's",
            )?;
        }
        if let (RecordBinding::Unrequested, Some(a), Some(r)) = (binding, self.attempt_id, receipt)
        {
            ensure(
                r.attempt_id == a,
                "Paykit wire 4.11: the receipt isn't of the record's attempt_id",
            )?;
        }
        // Paykit wire 5.3: the destination's amount is the stored proof's.
        if let (RecordBinding::Unrequested, Some(amount), Some(Body::PaymentProof(p))) = (
            binding,
            self.unrequested.as_ref().and_then(|u| u.amount.as_ref()),
            self.proof.as_ref().map(StoredMessage::body),
        ) {
            ensure(
                p.amount.as_ref() == Some(amount),
                "Paykit wire 5.3: unrequested.amount isn't the stored proof's",
            )?;
        }
        let recurring_request = matches!(
            self.request.as_ref().map(StoredMessage::body),
            Some(Body::PaymentRequest(r)) if r.is_recurring()
        );
        if !self.attempted_periods.is_empty() {
            ensure(
                self.side == ActivitySide::Payer && recurring_request,
                "Paykit wire 5.3: attempted_periods only in a payer's request record of a recurring request",
            )?;
        }
        if !self.reassociations.is_empty() {
            ensure(
                self.side == ActivitySide::Payer
                    && recurring_request
                    && self.allowance_id.is_some(),
                "Paykit wire 5.3: reassociations only in a payer's request record of a recurring request, beside allowance_id",
            )?;
        }
        // Paykit wire 5.3: each from_period is a Billing Period start of the stored request,
        // so no entry moves a time that names no period.
        if let Some(Body::PaymentRequest(PaymentRequest {
            recurrence: Some(recurrence),
            ..
        })) = self.request.as_ref().map(StoredMessage::body)
        {
            ensure(
                self.reassociations
                    .iter()
                    .all(|r| is_period_start(recurrence, r.from_period)),
                "Paykit wire 5.3: a reassociation's from_period isn't a Billing Period start of the request",
            )?;
        }
        if self.manual_only {
            let one_time_request =
                matches!(binding, RecordBinding::Request { .. }) && !recurring_request;
            let period = matches!(binding, RecordBinding::Period { .. });
            ensure(
                self.side == ActivitySide::Payer && (one_time_request || period),
                "Paykit wire 5.3: manual_only only in a payer's request record of a one-time request, or a payer's period record",
            )?;
        }
        Ok(())
    }

    /// The period details a period record holds (Paykit wire 4.9, 5.3): at most 8 in all,
    /// `earlier_details` in order, no two as late as each other, and `period_details` the
    /// latest of them, or, once the record holds a proof that names details, those.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] naming the rule: the record is invalid.
    fn check_details(&self) -> Result<()> {
        use std::cmp::Ordering;
        if self.earlier_details.is_empty() {
            return Ok(());
        }
        ensure(
            self.earlier_details.len() < MAX_PERIOD_DETAILS,
            "Paykit wire 5.3: earlier_details holds at most 7 details",
        )?;
        let Some(current) = &self.period_details else {
            return invalid("Paykit wire 5.3: earlier_details without period_details");
        };
        ensure(
            self.earlier_details.windows(2).all(|w| {
                matches!(w, [a, b] if crate::bodies::period_details_order(&a.message, &b.message) == Ordering::Less)
            }),
            "Paykit wire 5.3: earlier_details isn't in the order of Paykit wire 4.9, each as late as no other",
        )?;
        let named = self
            .proof
            .as_ref()
            .and_then(|p| p.message.refs.get(1).copied());
        for earlier in &self.earlier_details {
            let order = crate::bodies::period_details_order(&current.message, &earlier.message);
            ensure(
                order != Ordering::Equal,
                "Paykit wire 4.9: a period record holds two details as late as each other",
            )?;
            ensure(
                named.is_some() || order == Ordering::Greater,
                "Paykit wire 5.3: period_details isn't the latest details held, and no proof names it",
            )?;
        }
        Ok(())
    }

    /// What the record is, from its messages' signed fields (Paykit wire 5.3 "Binding"): a
    /// request record when it stores the request, whose messages all name it and no period;
    /// a period record when any message names a request or a period, or `request_hash` or
    /// `billing_period` is set, every message then naming the same request and period; else
    /// a payment without a request.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] for a record that breaks the binding: it is invalid.
    pub fn binding(&self) -> Result<RecordBinding> {
        let named: Vec<NamedBinding<'_>> = self
            .stored()
            .into_iter()
            .map(|(role, s)| {
                (
                    role,
                    named_request(role, s),
                    s.body.billing_period().copied(),
                )
            })
            .collect();
        if let Some(request) = &self.request {
            let hash = request.hash();
            for (_, h, p) in &named {
                ensure(
                    *h == Some(hash),
                    "Paykit wire 5.3: a stored message doesn't name the record's request",
                )?;
                ensure(
                    p.is_none(),
                    "Paykit wire 5.3: a message in a request record names a Billing Period",
                )?;
            }
            ensure(
                self.request_hash.is_none_or(|h| h == hash),
                "Paykit wire 5.3: request_hash isn't the stored request's",
            )?;
            return Ok(RecordBinding::Request { request_hash: hash });
        }
        let names_any = named.iter().any(|(_, h, p)| h.is_some() || p.is_some());
        if !names_any && self.request_hash.is_none() && self.billing_period.is_none() {
            return Ok(RecordBinding::Unrequested);
        }
        let Some((_, Some(h), Some(p))) = named.first().copied() else {
            return invalid(
                "Paykit wire 5.3: a period record's message misses a binding field, or it has none",
            );
        };
        for (_, hk, pk) in &named {
            ensure(
                *hk == Some(h) && *pk == Some(p),
                "Paykit wire 5.3: a message names another request or period than the record's",
            )?;
        }
        ensure(
            self.request_hash.is_none_or(|x| x == h) && self.billing_period.is_none_or(|x| x == p),
            "Paykit wire 5.3: request_hash or billing_period isn't the messages'",
        )?;
        Ok(RecordBinding::Period {
            request_hash: h,
            billing_period: p,
        })
    }

    /// The members only some records carry (Paykit wire 5.3), against the record's binding
    /// and side: `request_hash` and `billing_period` only in a period record (as
    /// `earlier_details`, whose details each name their period, so the binding refuses them
    /// anywhere else), `attempt_id` and `unrequested` only in that of a payment without a request,
    /// and `allowance_id` and `reassociations` only in a payer's request record.
    /// `attempted_periods`, `reassociations` and `manual_only` are checked with the request
    /// they concern.
    ///
    /// Security: these members are unsigned. A record that carries one where its messages
    /// don't put it is invalid, so a writer can't make a record that reads as two kinds at
    /// once, such as a request record that also names an attempt of a payment without one.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] naming the member: the record is invalid.
    fn check_members(&self, binding: &RecordBinding) -> Result<()> {
        let period = matches!(binding, RecordBinding::Period { .. });
        let unrequested = matches!(binding, RecordBinding::Unrequested);
        let payer_request =
            matches!(binding, RecordBinding::Request { .. }) && self.side == ActivitySide::Payer;
        ensure(
            period || self.request_hash.is_none(),
            "Paykit wire 5.3: request_hash only in a period record",
        )?;
        ensure(
            period || self.billing_period.is_none(),
            "Paykit wire 5.3: billing_period only in a period record",
        )?;
        ensure(
            unrequested || self.attempt_id.is_none(),
            "Paykit wire 5.3: attempt_id only in the record of a payment without a request",
        )?;
        ensure(
            unrequested || self.unrequested.is_none(),
            "Paykit wire 5.3: unrequested only in the record of a payment without a request",
        )?;
        ensure(
            payer_request || self.allowance_id.is_none(),
            "Paykit wire 5.3: allowance_id only in a payer's request record",
        )?;
        ensure(
            payer_request || self.reassociations.is_empty(),
            "Paykit wire 5.3: reassociations only in a payer's request record",
        )
    }

    /// `refs` (Paykit wire 4.3): a response, cancellation and period details name the
    /// request first; a proof the request, then the period details it used; a receipt the
    /// proof, then the request.
    fn check_refs(&self, binding: &RecordBinding) -> Result<()> {
        let request_hash = match binding {
            RecordBinding::Request { request_hash }
            | RecordBinding::Period { request_hash, .. } => Some(*request_hash),
            RecordBinding::Unrequested => None,
        };
        for (role, stored) in self.stored() {
            let refs = &stored.message.refs;
            match role {
                "response" | "cancellation" | "period_details" | "earlier_details" | "proof"
                    if request_hash.is_some() =>
                {
                    ensure(
                        refs.first() == request_hash.as_ref(),
                        "Paykit wire 4.3: refs don't name the request first",
                    )?;
                }
                "receipt" => {
                    if let Some(proof) = &self.proof {
                        ensure(
                            refs.first() == Some(&proof.hash()),
                            "Paykit wire 4.11: a receipt's refs don't name the proof first",
                        )?;
                    }
                    if let Some(h) = &request_hash {
                        ensure(
                            refs.get(1) == Some(h),
                            "Paykit wire 4.11: a receipt's refs don't name the request second",
                        )?;
                    }
                }
                _ => {}
            }
        }
        if let (Some(proof), Some(details)) = (&self.proof, &self.period_details) {
            if let Some(used) = proof.message.refs.get(1) {
                ensure(
                    *used == details.hash(),
                    "Paykit wire 5.3: the stored period details aren't the ones the proof used",
                )?;
            }
        }
        Ok(())
    }

    /// The natural key the record sits at (Paykit wire 1.5, 5.3), from its binding: a
    /// request record at `lp(payee, request hash)`, a period record at `lp(payee, request
    /// hash, u64(period start))`, a payer's payment without a request at `lp(own, attempt
    /// ID)`, and a payee's at `lp(Payment Endpoint Identifier, settlement ID)` of its proof.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] for a record without what its key needs, such as a proof whose
    /// method gives no settlement ID, which isn't credited.
    pub fn natural_key(&self, own: &[u8; 32]) -> Result<Vec<u8>> {
        self.natural_key_with(own, &crate::derive::settlement_id)
    }

    /// [`ActivityRecord::natural_key`], with a payee's settlement ID read from its proof's
    /// `proof` object by `settlement`: the reader of the payment method's rail, so a method
    /// this crate doesn't know names its settlement too (Paykit wire 1.5).
    ///
    /// # Errors
    ///
    /// As [`ActivityRecord::natural_key`].
    pub fn natural_key_with(
        &self,
        own: &[u8; 32],
        settlement: &crate::derive::SettlementIdReader<'_>,
    ) -> Result<Vec<u8>> {
        let (_, payee) = self.payer_payee(own);
        Ok(match self.binding()? {
            RecordBinding::Request { request_hash } => {
                natural_key::request_record(&payee, &request_hash)
            }
            RecordBinding::Period {
                request_hash,
                billing_period,
            } => natural_key::period_record(&payee, &request_hash, billing_period.start_secs()),
            RecordBinding::Unrequested => match self.side {
                ActivitySide::Payer => natural_key::payer_payment_record(
                    own,
                    &self.attempt_id.ok_or_else(|| {
                        Error::Invalid(
                            "Paykit wire 5.3: a payment without a request names attempt_id".into(),
                        )
                    })?,
                ),
                ActivitySide::Payee => {
                    let Some(Body::PaymentProof(proof)) =
                        self.proof.as_ref().map(StoredMessage::body)
                    else {
                        return invalid(
                            "Paykit wire 5.3: a payee's record of a payment without a request holds its proof",
                        );
                    };
                    let settlement = settlement(&proof.proof).ok_or_else(|| {
                        Error::Invalid(
                            "Paykit wire 1.5: a proof whose method gives no settlement ID isn't credited".into(),
                        )
                    })?;
                    natural_key::payee_payment_record(
                        &proof.payment_endpoint_identifier,
                        &settlement,
                    )
                }
            },
        })
    }

    /// The record key, recomputed with the reader's own naming key under the policy of
    /// `label` (Paykit wire 5.1, 5.3); a payer's record keeps the fixed policy whatever the
    /// label.
    ///
    /// # Errors
    ///
    /// As [`ActivityRecord::natural_key`].
    pub fn record_key(
        &self,
        own: &[u8; 32],
        naming_key: &NamingKey,
        label: Option<&Label>,
    ) -> Result<[u8; 32]> {
        self.side
            .collection()
            .record_key(naming_key, &self.natural_key(own)?, label)
    }

    /// "Labels" (Paykit spec 6, Paykit wire 5.3): in a `payee_activity` record under the
    /// policy of `label`, the stored request and every message from the payer carry that
    /// label; under the fixed policy (`None`) none does. The payee's own messages but the
    /// request aren't checked. A `payer_activity` record isn't checked.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`]: the record is invalid.
    pub fn check_label(&self, label: Option<&Label>) -> Result<()> {
        if self.side != ActivitySide::Payee {
            return Ok(());
        }
        for (role, stored) in self.stored() {
            if role != "request" && stored.sender != self.counterparty {
                continue;
            }
            ensure(
                stored.body.label() == label,
                "Paykit wire 5.3: a message names another label than the record's policy",
            )?;
        }
        Ok(())
    }

    /// Read an activity record at `key` under `policy_label` (Paykit wire 5.1, 5.3): the
    /// record reads, its labels are its policy's, and it sits at the key recomputed with the
    /// reader's own naming key.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] or [`Error::Switch`]: the reader doesn't act on it.
    pub fn bind(
        json: &[u8],
        own: &[u8; 32],
        side: ActivitySide,
        naming_key: &NamingKey,
        key: &[u8; 32],
        policy_label: Option<&Label>,
    ) -> Result<Self> {
        let record = Self::read(json, own, side)?;
        record.check_label(policy_label)?;
        ensure(
            &record.record_key(own, naming_key, policy_label)? == key,
            "Paykit wire 5.3: the record isn't at the key its messages name",
        )?;
        Ok(record)
    }

    /// Whether the record reflects a message: one it stores always, else one `msg_ids`
    /// names (Paykit wire 5.3).
    pub fn reflects(&self, msg_id: &str) -> bool {
        self.stored()
            .iter()
            .any(|(_, s)| s.message.msg_id == msg_id)
            || self.msg_ids.iter().any(|x| x == msg_id)
    }

    /// Whether the record carries the proof of a payment: then no app attempts its
    /// obligation, whatever the claim shows (Paykit spec 7.2).
    pub fn shows_paid(&self) -> bool {
        self.proof.is_some()
    }

    /// The Allowance a payer's request record names for the Billing Period starting at
    /// `start`, in Unix seconds (Paykit spec 8.3, Paykit wire 5.3): that of the last entry of
    /// `reassociations` starting at or before it, else `allowance_id`. An attempt on the
    /// period counts under it, whether made early or not.
    pub fn allowance_for(&self, start: u64) -> Option<&AllowanceId> {
        self.reassociations
            .iter()
            .rev()
            .find(|r| r.from_period <= start)
            .map(|r| &r.allowance_id)
            .or(self.allowance_id.as_ref())
    }

    /// "Seen roles", "Seen Allowance" and "Seen marks" (Paykit wire 5.3, Switch spec 8.2): a
    /// reader that holds `self` ignores, and shows with its writer, a later write of the same
    /// record that drops or replaces a message in a role it saw; that drops period details it
    /// saw, in `period_details` or `earlier_details`; that changes or drops the
    /// `allowance_id` or an entry of `reassociations` it saw, or adds an entry not after the
    /// last it saw; or that drops a `manual_only` it saw. These hold whatever version the
    /// write builds on, so they apply to
    /// a reader that holds an older one too. A later write moves `period_details` only to
    /// later details, or, when it stores the proof, to the details the proof names: both
    /// records are valid, and the later keeps every details seen.
    ///
    /// A reader that holds the version the write builds on checks
    /// [`ActivityRecord::check_later_write_built_on`] instead.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] naming the rule `later` breaks.
    pub fn check_later_write(&self, later: &ActivityRecord) -> Result<()> {
        // Records built in code are held to the reader's rules first (Paykit wire 5.3).
        self.check()?;
        later.check()?;
        self.check_same_record(later)?;
        let later_roles = later.stored();
        for (role, seen) in self.stored() {
            if role == "period_details" || role == EARLIER_DETAILS.0 {
                continue;
            }
            let now = later_roles
                .iter()
                .find(|(r, _)| *r == role)
                .map(|(_, s)| *s);
            ensure(
                now.is_some_and(|n| n.hash() == seen.hash()),
                "Paykit wire 5.3: a later write drops or replaces a message in a role seen",
            )?;
        }
        // Paykit wire 4.9: each details stays valid until its own expiry, so none replaces
        // another. Other details as late as those seen, an equivocation, can't come beside
        // them: a valid record holds no two details as late as each other.
        let (seen, now) = (self.details(), later.details());
        ensure(
            seen.iter()
                .all(|s| now.iter().any(|n| n.hash() == s.hash())),
            "Paykit wire 5.3: a later write drops period details seen",
        )?;
        // Where period_details may move follows: a valid record holds the latest details in
        // it unless its proof names others, and a later write keeps every details seen, so
        // only the write that stores a proof naming earlier details moves it back.
        if let Some(seen) = &self.allowance_id {
            ensure(
                later.allowance_id.as_ref() == Some(seen),
                "Paykit wire 5.3: a later write changes or drops the allowance_id seen",
            )?;
        }
        // Entries only follow those seen: the list is strictly ascending, so an entry added
        // after them is after the last one seen.
        ensure(
            later.reassociations.starts_with(&self.reassociations),
            "Paykit wire 5.3: a later write changes or drops a reassociation seen",
        )?;
        ensure(
            !self.manual_only || later.manual_only,
            "Paykit wire 5.3: a later write drops the manual_only seen",
        )
    }

    /// [`ActivityRecord::check_later_write`] for a reader that holds the version `later`
    /// builds on, `self`: the write's `expected_version` is the version the reader holds.
    /// Such a reader also ignores and flags a write that adds a reassociation whose
    /// `from_period` is at or before a period of `self`'s `attempted_periods`, so a period
    /// attempted early stays under the Allowance it was attempted under (Paykit spec 8.2,
    /// 8.3, Paykit wire 5.3 "Seen Allowance").
    ///
    /// A reader that holds another version doesn't check that rule: the version the write
    /// builds on may have dropped a period its own lists, once its attempt ended, and a
    /// period attempted after the reassociation, early or not, counts under it.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] naming the rule `later` breaks.
    pub fn check_later_write_built_on(&self, later: &ActivityRecord) -> Result<()> {
        self.check_later_write(later)?;
        let added = later.reassociations.get(self.reassociations.len()..);
        ensure(
            added.into_iter().flatten().all(|entry| {
                self.attempted_periods
                    .iter()
                    .all(|start| entry.from_period > *start)
            }),
            "Paykit wire 5.3: a later write adds a reassociation at or before an attempted period",
        )
    }

    /// Two versions of one activity record (Paykit wire 5.3): the same collection and
    /// counterparty, the same binding (request, Billing Period, or a payment without a
    /// request), and, for a payment without a request, the `attempt_id` held.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] for another record.
    fn check_same_record(&self, later: &ActivityRecord) -> Result<()> {
        ensure(
            self.side == later.side
                && self.counterparty == later.counterparty
                && self.binding()? == later.binding()?
                && self.attempt_id.is_none_or(|a| later.attempt_id == Some(a)),
            "Paykit wire 5.3: a later write is another record's",
        )
    }

    /// "Attempted periods" (Paykit wire 5.3): a reader that holds the version a write was
    /// built on (`self`) checks every listed period `later` drops: the write's `conditions`
    /// carry a `claim_step` condition on that period's claim, the obligation of (payee,
    /// request hash, start) with the reader's naming key. An entry that names no Billing
    /// Period start of the request fences nothing, so it is dropped without a condition: no
    /// claim stands behind it. Returns the entries dropped.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] for a dropped period without its condition: the write is ignored
    /// and flagged, and the reader keeps the period listed.
    pub fn check_dropped_periods(
        &self,
        later: &ActivityRecord,
        conditions: &[Condition],
        own: &[u8; 32],
        naming_key: &NamingKey,
    ) -> Result<Vec<u64>> {
        // Records built in code are held to the reader's rules first (Paykit wire 5.3).
        self.check()?;
        later.check()?;
        self.check_same_record(later)?;
        let RecordBinding::Request { request_hash } = self.binding()? else {
            return invalid("Paykit wire 5.3: attempted_periods belong to a request record");
        };
        let (_, payee) = self.payer_payee(own);
        let recurrence = match self.request.as_ref().map(StoredMessage::body) {
            Some(Body::PaymentRequest(r)) => r.recurrence.as_ref(),
            _ => None,
        };
        let mut dropped = Vec::new();
        for start in &self.attempted_periods {
            if later.attempted_periods.contains(start) {
                continue;
            }
            if !recurrence.is_some_and(|r| is_period_start(r, *start)) {
                dropped.push(*start);
                continue;
            }
            let claim = request_obligation(naming_key, &payee, &request_hash, *start);
            ensure(
                conditions.iter().any(|c| {
                    matches!(c, Condition::ClaimStep { claim_key, .. } if *claim_key == claim)
                }),
                "Paykit wire 5.3: a write drops a listed period without a claim_step condition on its claim",
            )?;
            dropped.push(*start);
        }
        Ok(dropped)
    }

    /// Write the record value: every member, `null` for absent, but `allowance_id` when
    /// absent, `manual_only` when false, and `attempted_periods`, `earlier_details`,
    /// and `reassociations` when empty, which are left out; `msg_ids` as its
    /// canonical list, [`bounded_msg_ids`] of the stored messages and `msg_ids`; and the
    /// members it doesn't know as it read them, [`ActivityRecord::unknown`] (Paykit wire 5,
    /// 5.3). The record's rules are checked first, as a reader checks them, so the writer
    /// never writes a record its readers refuse.
    ///
    /// # Errors
    ///
    /// [`Error::Validation`] for a record that breaks a rule of Paykit wire 5.3, and
    /// [`Error::Invalid`] if the value can't be written.
    pub fn to_json(&self) -> Result<Vec<u8>> {
        self.check().map_err(Error::into_validation)?;
        self.json_unchecked()
    }

    /// Write the record value without its rules, for the vector tests' rejection cases, which
    /// readers must refuse (Paykit wire 5.3). Test-only: no production build has it.
    #[cfg(feature = "test-vectors")]
    #[doc(hidden)]
    pub fn to_json_unchecked(&self) -> Result<Vec<u8>> {
        self.json_unchecked()
    }

    fn json_unchecked(&self) -> Result<Vec<u8>> {
        let msg_ids = written_msg_ids(&self.stored_msg_ids(), &self.msg_ids)?;
        let out = ActivityOut {
            v: 1,
            counterparty: pubky_id(&self.counterparty),
            request: self.request.as_ref().map(StoredMessage::out),
            response: self.response.as_ref().map(StoredMessage::out),
            cancellation: self.cancellation.as_ref().map(StoredMessage::out),
            proof: self.proof.as_ref().map(StoredMessage::out),
            receipt: self.receipt.as_ref().map(StoredMessage::out),
            unrequested: self.unrequested.as_ref().map(unrequested_json),
            attempt_id: self.attempt_id.map(AttemptId::to_text),
            msg_ids: &msg_ids,
            status: self.status.as_deref(),
            period_details: self.period_details.as_ref().map(StoredMessage::out),
            request_hash: self.request_hash.map(|h| b64(&h)),
            billing_period: self.billing_period.map(|p| p.to_json()),
            allowance_id: self.allowance_id.as_ref().map(AllowanceId::as_str),
            attempted_periods: &self.attempted_periods,
            earlier_details: self
                .earlier_details
                .iter()
                .map(StoredMessage::out)
                .collect(),
            reassociations: self
                .reassociations
                .iter()
                .map(|r| ReassociationOut {
                    allowance_id: r.allowance_id.as_str(),
                    from_period: r.from_period,
                })
                .collect(),
            manual_only: self.manual_only,
        };
        let json = serde_json::to_vec(&out)
            .map_err(|_| Error::Invalid(format!("{ACTIVITY}: can't be written")))?;
        self.unknown.write_into(json, &ACTIVITY_SHAPE, &[])
    }
}

/// The condition the `record.put` adding a reassociation for the Billing Period starting at
/// `from_period`, in Unix seconds, carries (Paykit wire 5.3 "Seen Allowance"):
/// `server_time_in(null, from_period)`, so the period hasn't started on the service's clock.
/// The bound is the period's second, `from_period` seconds: a period starting within that
/// second, such as at `...00.500Z`, starts at or after it, so the write never applies once
/// the period has started.
///
/// # Errors
///
/// [`Error::Validation`] for a start past the last time Switch wire 1.6 writes.
pub fn reassociation_condition(from_period: u64) -> Result<Condition> {
    let end = from_period
        .checked_mul(1000)
        .filter(|ms| *ms <= pubky_switch_core::time::MAX_TIME_MS)
        .ok_or_else(|| {
            Error::Validation(
                "Paykit wire 5.3: from_period past the last time Switch wire 1.6 writes".into(),
            )
        })?;
    Ok(Condition::ServerTimeIn {
        start: None,
        end: Some(end),
    })
}

/// A single-use endpoint used, in a `payment_lists` record (Paykit wire 5.4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsedEndpoint {
    /// The list version it was in.
    pub version: u64,
    /// The identifier.
    pub endpoint: PaymentEndpointIdentifier,
    /// The attempt whose batch reserved it.
    pub attempt_id: AttemptId,
    /// Where it was read, for the members its writer doesn't know (Paykit wire 5).
    pub origin: Origin,
}

/// A `payment_lists` record value (Paykit wire 5.4): the latest Private Payment List of one
/// payee app, and the single-use endpoints used.
#[derive(Clone, Debug)]
pub struct PaymentListRecord {
    /// The payee.
    pub payee: [u8; 32],
    /// The publishing app.
    pub app_id: [u8; 16],
    /// The latest `private_payment_list`.
    pub list: StoredMessage,
    /// Single-use endpoints used.
    pub used: Vec<UsedEndpoint>,
    /// The `msg_id`s the record reflects.
    pub msg_ids: Vec<String>,
    /// The members the writer doesn't know, kept as read (Paykit wire 5).
    pub unknown: UnknownMembers,
}

const LISTS: &str = "Paykit wire 5.4: payment_lists";

#[derive(Serialize)]
struct ListOut<'a> {
    v: u64,
    payee: String,
    app_id: String,
    list: StoredOut,
    used: Vec<UsedOut>,
    msg_ids: &'a [String],
}

#[derive(Serialize)]
struct UsedOut {
    version: u64,
    endpoint: String,
    attempt_id: String,
}

impl PaymentListRecord {
    /// Read a `payment_lists` value held by `own` (Paykit wire 5.4): the stored list
    /// verifies, addressed to this identity, from `payee` and its app `app_id`.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] or [`Error::Switch`] naming the rule.
    pub fn read(json: &[u8], own: &[u8; 32]) -> Result<Self> {
        let map = parse_object(json, LISTS)?;
        let obj = Obj::new(&map, LISTS);
        check_v1(&obj)?;
        let payee = obj.req_pubky("payee")?;
        let app_id = obj.req_app_id("app_id")?;
        let list = StoredMessage::read(obj.req("list")?, own)?;
        let msg_ids = read_msg_ids(&obj, &msg_ids_of([&list]))?;
        let (unknown, origins) = UnknownMembers::read(&map, &LIST_SHAPE);
        let mut used = Vec::new();
        for (i, item) in obj.opt_arr("used")?.into_iter().flatten().enumerate() {
            let u = Obj::of(item, LISTS, "used")?;
            used.push(UsedEndpoint {
                version: u.req_int("version")?,
                endpoint: PaymentEndpointIdentifier::parse(u.req_str("endpoint")?)?,
                attempt_id: AttemptId::from_bytes(u.req_b64::<16>("attempt_id")?),
                origin: origins.at(&format!("/used/{i}")),
            });
        }
        let record = Self {
            payee,
            app_id,
            list,
            used,
            msg_ids,
            unknown,
        };
        record.check()?;
        Ok(record)
    }

    /// The record's rules (Paykit wire 5.4), the same for the reader and the writer: the
    /// stored list is a `private_payment_list` from `payee` and its app `app_id`, addressed
    /// to another identity.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] naming the rule.
    fn check(&self) -> Result<()> {
        ensure(
            self.list.body.kind() == Kind::PrivatePaymentList,
            "Paykit wire 5.4: list holds another kind than private_payment_list",
        )?;
        ensure(
            self.list.sender == self.payee && self.list.message.to != self.payee,
            "Paykit wire 5.4: the stored list's sender identity isn't payee",
        )?;
        ensure(
            self.list.message.from.app_id == self.app_id,
            "Paykit wire 5.4: the stored list's sender app isn't app_id",
        )
    }

    /// The held-to-later rule of a `payment_lists` record (Paykit wire 4.13, 5.4, Switch spec
    /// 8.2): a reader that holds `self` ignores, and shows with its writer, a later write of
    /// the record whose list is older than the one it holds, or of the same `version` but
    /// another message, by hash; and one that drops a single-use endpoint it saw used, at any
    /// version, which could then be paid twice. A later write holding the same list, its
    /// `used` grown, or a list of a greater `version`, is taken.
    ///
    /// Security: a publishing app's versions only grow (Paykit wire 4.13), so two lists of one
    /// version with other content are an equivocation; neither replaces the one held.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] naming the rule `later` breaks.
    pub fn check_later_write(&self, later: &PaymentListRecord) -> Result<()> {
        // Records built in code are held to the reader's rules first (Paykit wire 5.4).
        self.check()?;
        later.check()?;
        ensure(
            self.payee == later.payee && self.app_id == later.app_id,
            "Paykit wire 5.4: a later write is another payee app's list record",
        )?;
        let version = |r: &PaymentListRecord| match r.list.body() {
            Body::PrivatePaymentList(l) => Ok(l.version),
            _ => invalid("Paykit wire 5.4: list holds another kind than private_payment_list"),
        };
        let (held, now) = (version(self)?, version(later)?);
        ensure(
            now > held || (now == held && later.list.hash() == self.list.hash()),
            "Paykit wire 4.13: a later write holds an older list, or another list of the same version",
        )?;
        ensure(
            self.used.iter().all(|u| later.used.contains(u)),
            "Paykit wire 5.4: a later write drops a single-use endpoint seen used",
        )
    }

    /// The record key, with the reader's own naming key (Paykit wire 1.5): `lp(payee,
    /// app ID)` in `payment_lists`.
    ///
    /// # Errors
    ///
    /// [`Error::Switch`] if the policy hash can't be computed.
    pub fn record_key(&self, naming_key: &NamingKey) -> Result<[u8; 32]> {
        Collection::PaymentLists.record_key(
            naming_key,
            &natural_key::payment_list(&self.payee, &self.app_id),
            None,
        )
    }

    /// The binding rule (Paykit wire 5.4, Switch spec 8.2): read the record at `key` and act
    /// on it only if it is the key derived from `payee` and `app_id`.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] or [`Error::Switch`]: the reader doesn't act on it and flags it.
    pub fn bind(
        json: &[u8],
        own: &[u8; 32],
        naming_key: &NamingKey,
        key: &[u8; 32],
    ) -> Result<Self> {
        let record = Self::read(json, own)?;
        ensure(
            &record.record_key(naming_key)? == key,
            "Paykit wire 5.4: the record isn't at the key derived from payee and app_id",
        )?;
        Ok(record)
    }

    /// Write the record value, `msg_ids` as its canonical list, [`bounded_msg_ids`] of the
    /// stored list and `msg_ids`, and the members it doesn't know as it read them (Paykit wire
    /// 5, 5.3, 5.4), after the record's rules, as a reader checks them.
    ///
    /// # Errors
    ///
    /// [`Error::Validation`] for a record that breaks a rule of Paykit wire 5.4, and
    /// [`Error::Invalid`] if the value can't be written.
    pub fn to_json(&self) -> Result<Vec<u8>> {
        self.check().map_err(Error::into_validation)?;
        let msg_ids = written_msg_ids(&msg_ids_of([&self.list]), &self.msg_ids)?;
        let out = ListOut {
            v: 1,
            payee: pubky_id(&self.payee),
            app_id: zbase32_encode(&self.app_id),
            list: self.list.out(),
            used: self
                .used
                .iter()
                .map(|u| UsedOut {
                    version: u.version,
                    endpoint: u.endpoint.as_str().into(),
                    attempt_id: u.attempt_id.to_text(),
                })
                .collect(),
            msg_ids: &msg_ids,
        };
        let json = serde_json::to_vec(&out)
            .map_err(|_| Error::Invalid(format!("{LISTS}: can't be written")))?;
        let origins: Vec<(String, Origin)> = self
            .used
            .iter()
            .enumerate()
            .map(|(i, u)| (format!("/used/{i}"), u.origin))
            .collect();
        self.unknown.write_into(json, &LIST_SHAPE, &origins)
    }
}

/// One entry of an Allowance record's grant history (Paykit wire 5.4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrantEntry {
    /// The Allowance record version the write adding the grant built on: its
    /// `expected_version`, `{0, 0}` for a new record. The grant signs it.
    pub built_on: RecordVersion,
    /// The grant.
    pub grant: ExecutorGrant,
    /// Where it was read, for the members its writer doesn't know (Paykit wire 5).
    pub origin: Origin,
}

/// An `allowances` record value (Paykit wire 5.4), on either side of an Allowance.
#[derive(Clone, Debug)]
pub struct AllowanceRecord {
    /// The other identity.
    pub counterparty: [u8; 32],
    /// The Allowance.
    pub allowance_id: AllowanceId,
    /// The proposal.
    pub proposal: StoredMessage,
    /// The acceptance or rejection.
    pub response: Option<StoredMessage>,
    /// The end.
    pub end: Option<StoredMessage>,
    /// A second proposal under the same `allowance_id`, with another hash than `proposal`,
    /// from either side, or a second response, with another hash than `response`, from the
    /// proposal's recipient: the Allowance is conflicted (Paykit spec 8.3). Nothing binds or
    /// is paid under it, and no later write drops it.
    pub conflicted: Option<StoredMessage>,
    /// The current grant: the last entry of `grants`.
    pub grant: Option<ExecutorGrant>,
    /// The grant history, oldest first.
    pub grants: Vec<GrantEntry>,
    /// The `msg_id`s the record reflects.
    pub msg_ids: Vec<String>,
    /// The members the writer doesn't know, kept as read (Paykit wire 5).
    pub unknown: UnknownMembers,
}

const ALLOWANCES: &str = "Paykit wire 5.4: allowances";

/// The kinds `conflicted` may hold (Paykit spec 8.3): a second proposal or a second response.
const CONFLICTING: [Kind; 3] = [
    Kind::AllowanceProposal,
    Kind::AllowanceAcceptance,
    Kind::AllowanceRejection,
];

#[derive(Serialize)]
struct AllowanceOut<'a> {
    v: u64,
    counterparty: String,
    allowance_id: &'a str,
    proposal: StoredOut,
    response: Option<StoredOut>,
    end: Option<StoredOut>,
    // Writers leave it out when absent (Paykit wire 5.4).
    #[serde(skip_serializing_if = "Option::is_none")]
    conflicted: Option<StoredOut>,
    grant: Option<Value>,
    grants: Vec<Value>,
    msg_ids: &'a [String],
}

/// Read a stored message of an `allowances` record (Paykit wire 5.4 "Binding"): between
/// `own` and `counterparty`, from `from` when its kind fixes the side, addressed to the
/// other side, of one of `kinds`, and naming `allowance_id`.
fn allowance_message(
    value: &Value,
    own: &[u8; 32],
    counterparty: &[u8; 32],
    from: Option<&[u8; 32]>,
    kinds: &[Kind],
    allowance_id: &AllowanceId,
) -> Result<StoredMessage> {
    let sender = Obj::of(value, ALLOWANCES, "stored message")?.req_pubky("sender")?;
    ensure(
        &sender == own || &sender == counterparty,
        "Paykit wire 5.4: a stored message isn't between this identity and counterparty",
    )?;
    if let Some(from) = from {
        ensure(
            &sender == from,
            "Paykit wire 5.4: a stored message isn't from the side its kind requires",
        )?;
    }
    let to = if &sender == own { counterparty } else { own };
    let stored = StoredMessage::read(value, to)?;
    ensure(
        kinds.contains(&stored.body.kind()),
        "Paykit wire 5.4: a member holds another kind",
    )?;
    let named = match &stored.body {
        Body::AllowanceProposal(p) => &p.allowance_id,
        Body::AllowanceAcceptance(r) | Body::AllowanceRejection(r) | Body::AllowanceEnd(r) => {
            &r.allowance_id
        }
        _ => return invalid("Paykit wire 5.4: a member holds another kind"),
    };
    ensure(
        named == allowance_id,
        "Paykit wire 5.4: a stored message names another Allowance",
    )?;
    Ok(stored)
}

impl AllowanceRecord {
    /// Read an `allowances` value held by `own` (Paykit wire 5.4 "Binding"): there is a
    /// proposal, with valid terms; the response comes from the proposal's recipient and the
    /// end from either side, each addressed to the other; `conflicted`, when present, is
    /// another proposal, with valid terms and another hash, from either side, or, beside a
    /// response, another response, with another hash, from the proposal's recipient; every
    /// stored message names `allowance_id`; and the response, the end and a conflicting
    /// response refer to the proposal in `refs`. A grant
    /// doesn't affect whether the record is valid: the grant history is checked by
    /// [`AllowanceRecord::current_grant`] and [`AllowanceRecord::executor_at`].
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] or [`Error::Switch`] naming the rule.
    pub fn read(json: &[u8], own: &[u8; 32]) -> Result<Self> {
        let map = parse_object(json, ALLOWANCES)?;
        let obj = Obj::new(&map, ALLOWANCES);
        check_v1(&obj)?;
        let counterparty = obj.req_pubky("counterparty")?;
        let allowance_id = AllowanceId::parse(obj.req_str("allowance_id")?)?;
        let Some(proposal) = obj.opt("proposal") else {
            return invalid("Paykit wire 5.4: an allowances record holds its proposal");
        };
        let proposal = allowance_message(
            proposal,
            own,
            &counterparty,
            None,
            &[Kind::AllowanceProposal],
            &allowance_id,
        )?;
        let responder = proposal.message.to;
        let refers = |stored: &StoredMessage| {
            ensure(
                stored.message.refs.first() == Some(&proposal.hash()),
                "Paykit wire 4.16: a response or an end refers to the proposal in refs",
            )
        };
        let response = obj
            .opt("response")
            .map(|v| {
                allowance_message(
                    v,
                    own,
                    &counterparty,
                    Some(&responder),
                    &[Kind::AllowanceAcceptance, Kind::AllowanceRejection],
                    &allowance_id,
                )
            })
            .transpose()?;
        let end = obj
            .opt("end")
            .map(|v| {
                allowance_message(
                    v,
                    own,
                    &counterparty,
                    None,
                    &[Kind::AllowanceEnd],
                    &allowance_id,
                )
            })
            .transpose()?;
        let conflicted = obj
            .opt("conflicted")
            .map(|v| allowance_message(v, own, &counterparty, None, &CONFLICTING, &allowance_id))
            .transpose()?;
        for stored in response.iter().chain(end.iter()) {
            refers(stored)?;
        }
        let msg_ids = read_msg_ids(
            &obj,
            &msg_ids_of(
                std::iter::once(&proposal)
                    .chain(&response)
                    .chain(&end)
                    .chain(&conflicted),
            ),
        )?;
        let (unknown, origins) = UnknownMembers::read(&map, &ALLOWANCE_SHAPE);
        let mut grants = Vec::new();
        for (i, item) in obj.opt_arr("grants")?.into_iter().flatten().enumerate() {
            let entry = Obj::of(item, ALLOWANCES, "grants")?;
            grants.push(GrantEntry {
                built_on: RecordVersion {
                    generation: entry.req_int("generation")?,
                    version: entry.req_int("version")?,
                },
                grant: ExecutorGrant {
                    origin: origins.at(&format!("/grants/{i}/grant")),
                    ..ExecutorGrant::from_json(entry.req("grant")?)?
                },
                origin: origins.at(&format!("/grants/{i}")),
            });
        }
        let grant = obj
            .opt("grant")
            .map(ExecutorGrant::from_json)
            .transpose()?
            .map(|g| ExecutorGrant {
                origin: origins.at("/grant"),
                ..g
            });
        let record = Self {
            counterparty,
            allowance_id,
            proposal,
            response,
            end,
            conflicted,
            msg_ids,
            grant,
            grants,
            unknown,
        };
        record.check()?;
        Ok(record)
    }

    /// The record's rules (Paykit wire 5.4 "Binding"), the same for the reader and the
    /// writer: the stored messages are between the counterparty and one other identity, each
    /// of its member's kind and naming `allowance_id`; the response comes from the proposal's
    /// recipient; the response and the end refer to the proposal in `refs`; and `conflicted`
    /// is a proposal other than `proposal`, by hash, or, beside a response, a response other
    /// than it, by hash, from the proposal's recipient and referring to the proposal.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] naming the rule.
    fn check(&self) -> Result<()> {
        let members: [(Option<&StoredMessage>, &[Kind]); 4] = [
            (Some(&self.proposal), &[Kind::AllowanceProposal]),
            (
                self.response.as_ref(),
                &[Kind::AllowanceAcceptance, Kind::AllowanceRejection],
            ),
            (self.end.as_ref(), &[Kind::AllowanceEnd]),
            (self.conflicted.as_ref(), &CONFLICTING),
        ];
        let mut own: Option<[u8; 32]> = None;
        for (stored, kinds) in members {
            let Some(stored) = stored else { continue };
            let (sender, to) = (stored.sender, stored.message.to);
            let other = if sender == self.counterparty {
                to
            } else {
                sender
            };
            ensure(
                (sender == self.counterparty || to == self.counterparty)
                    && other != self.counterparty
                    && own.is_none_or(|o| o == other),
                "Paykit wire 5.4: a stored message isn't between this identity and counterparty",
            )?;
            own = Some(other);
            ensure(
                kinds.contains(&stored.body.kind()),
                "Paykit wire 5.4: a member holds another kind",
            )?;
            let named = match &stored.body {
                Body::AllowanceProposal(p) => &p.allowance_id,
                Body::AllowanceAcceptance(r)
                | Body::AllowanceRejection(r)
                | Body::AllowanceEnd(r) => &r.allowance_id,
                _ => return invalid("Paykit wire 5.4: a member holds another kind"),
            };
            ensure(
                named == &self.allowance_id,
                "Paykit wire 5.4: a stored message names another Allowance",
            )?;
        }
        if let Some(response) = &self.response {
            ensure(
                response.sender == self.proposal.message.to,
                "Paykit wire 5.4: a stored message isn't from the side its kind requires",
            )?;
            self.check_executor_app_id(response)?;
        }
        let proposal = self.proposal.hash();
        if let Some(conflicted) = &self.conflicted {
            // Paykit spec 8.3: a conflict is a second, different proposal, or a second,
            // different response. The same message again, a re-send, is the original one.
            if conflicted.body.kind() == Kind::AllowanceProposal {
                ensure(
                    conflicted.hash() != proposal,
                    "Paykit wire 5.4: conflicted holds another proposal than the record's",
                )?;
            } else {
                let Some(response) = &self.response else {
                    return invalid(
                        "Paykit wire 5.4: a conflicting response comes beside the record's response",
                    );
                };
                ensure(
                    conflicted.hash() != response.hash(),
                    "Paykit wire 5.4: conflicted holds another response than the record's",
                )?;
                ensure(
                    conflicted.sender == self.proposal.message.to,
                    "Paykit wire 5.4: a stored message isn't from the side its kind requires",
                )?;
                ensure(
                    conflicted.message.refs == [proposal],
                    "Paykit wire 4.16: a response or an end refers to the proposal in refs",
                )?;
                self.check_executor_app_id(conflicted)?;
            }
        }
        if let Some(response) = &self.response {
            ensure(
                response.message.refs == [proposal],
                "Paykit wire 4.16: a response or an end refers to the proposal in refs",
            )?;
        }
        if let Some(end) = &self.end {
            // Paykit wire 4.16: an end refers to the proposal and the acceptance; one that
            // refers to the proposal alone is its sender's withdrawal, before any response.
            let refs = &end.message.refs;
            match self.response.as_ref() {
                Some(acceptance) if acceptance.body.kind() == Kind::AllowanceAcceptance => ensure(
                    *refs == [proposal, acceptance.hash()],
                    "Paykit wire 4.16: an end refers to the proposal and the stored acceptance",
                )?,
                Some(_) => {
                    return invalid("Paykit wire 4.16: a rejected Allowance has no end");
                }
                None => ensure(
                    *refs == [proposal] && end.sender == self.proposal.sender,
                    "Paykit wire 4.16: an end before any response is the proposal's withdrawal by its sender",
                )?,
            }
        }
        Ok(())
    }

    /// Paykit wire 4.16: `executor_app_id` comes in an acceptance from the allower, the
    /// proposal's recipient when the proposer is the allowee.
    fn check_executor_app_id(&self, response: &StoredMessage) -> Result<()> {
        if let (Body::AllowanceAcceptance(r), Body::AllowanceProposal(p)) =
            (&response.body, &self.proposal.body)
        {
            ensure(
                r.executor_app_id.is_none() || p.proposer_role == ProposerRole::Allowee,
                "Paykit wire 4.16: executor_app_id only in an acceptance from the allower",
            )?;
        }
        Ok(())
    }

    /// The record key, with the reader's own naming key (Paykit wire 1.5): `lp(counterparty,
    /// allowance_id text)` in `allowances`.
    ///
    /// # Errors
    ///
    /// [`Error::Switch`] if the policy hash can't be computed.
    pub fn record_key(&self, naming_key: &NamingKey) -> Result<[u8; 32]> {
        Collection::Allowances.record_key(
            naming_key,
            &natural_key::allowance(&self.counterparty, &self.allowance_id),
            None,
        )
    }

    /// The key of this Allowance's `allowance_ledgers` record (Paykit wire 1.5, 5.5).
    ///
    /// # Errors
    ///
    /// [`Error::Switch`] if the policy hash can't be computed.
    pub fn ledger_key(&self, naming_key: &NamingKey) -> Result<[u8; 32]> {
        Collection::AllowanceLedgers.record_key(
            naming_key,
            &natural_key::allowance(&self.counterparty, &self.allowance_id),
            None,
        )
    }

    /// The binding rule (Paykit wire 5.4, Switch spec 8.2): read the record at `key`, and act
    /// on it only if the key is the one derived from `counterparty` and `allowance_id`.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] or [`Error::Switch`]: the reader doesn't act on it and flags it.
    pub fn bind(
        json: &[u8],
        own: &[u8; 32],
        naming_key: &NamingKey,
        key: &[u8; 32],
    ) -> Result<Self> {
        let record = Self::read(json, own)?;
        ensure(
            &record.record_key(naming_key)? == key,
            "Paykit wire 5.4: the record isn't at the key derived from counterparty and allowance_id",
        )?;
        Ok(record)
    }

    /// The terms hash (Paykit wire 1.5, 4.16), once the record holds an acceptance.
    pub fn terms_hash(&self) -> Option<[u8; 32]> {
        let response = self.response.as_ref()?;
        (response.body.kind() == Kind::AllowanceAcceptance)
            .then(|| terms_hash(self.proposal.plaintext.json(), response.plaintext.json()))
    }

    /// Whether the grant history is usable (Paykit wire 5.4): its versions strictly increase
    /// and it ends with an entry whose grant is `grant`. Otherwise every ledger check against
    /// it fails closed.
    pub fn history_usable(&self) -> bool {
        let increasing = self
            .grants
            .windows(2)
            .all(|w| w.first().map(|a| a.built_on) < w.get(1).map(|b| b.built_on));
        increasing
            && match (self.grants.last(), &self.grant) {
                (Some(last), Some(grant)) => &last.grant == grant,
                _ => false,
            }
    }

    /// Whether history entry `entry` names an executor (Paykit wire 5.4 "Valid entries"): its
    /// grant verifies at its version for this identity, counterparty and Allowance, and names
    /// the current terms hash. Returns the executor's certificate.
    fn valid_entry(&self, own: &[u8; 32], entry: &GrantEntry) -> Option<AppCert> {
        let terms = self.terms_hash()?;
        entry
            .grant
            .verify(
                own,
                &self.counterparty,
                &self.allowance_id,
                &terms,
                entry.built_on,
            )
            .ok()
    }

    /// The current grant (Paykit wire 5.4): the last entry of a usable history, if it
    /// verifies for this identity, the counterparty and the Allowance. Returns the grant and
    /// the executor's certificate; `None` means the record has no current grant. A
    /// conflicted record has none, so no app pays under it (Paykit spec 8.3); the history
    /// still names the executor at each version ([`AllowanceRecord::executor_at`]), so the
    /// outcomes of attempts already open are still recorded.
    pub fn current_grant(&self, own: &[u8; 32]) -> Option<(&ExecutorGrant, AppCert)> {
        if self.conflicted.is_some() || !self.history_usable() {
            return None;
        }
        let last = self.grants.last()?;
        self.valid_entry(own, last).map(|cert| (&last.grant, cert))
    }

    /// The executor at record version `version` (Paykit spec 8.3 "Who wrote it", Paykit wire
    /// 5.4): the app named by the last entry whose version is older than `version`. `None`
    /// when that entry doesn't verify, or there is none: no valid grant from that version
    /// on.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] for a history that isn't usable: every ledger check fails closed.
    pub fn executor_at(&self, own: &[u8; 32], version: RecordVersion) -> Result<Option<[u8; 16]>> {
        ensure(
            self.history_usable(),
            "Paykit wire 5.4: the grant history isn't usable; ledger checks fail closed",
        )?;
        Ok(self
            .grants
            .iter()
            .rev()
            .find(|e| e.built_on < version)
            .and_then(|e| self.valid_entry(own, e).map(|cert| cert.app_id())))
    }

    /// "Seen roles" of an Allowance record (Paykit spec 6, Paykit wire 5.4, Switch spec 8.2):
    /// a reader that holds `self` ignores, and shows with its writer, a later write of the
    /// record that drops or replaces the proposal, the response, the end or the conflicting
    /// proposal it saw, by hash, so a conflict is never cleared, or whose grant history
    /// doesn't start with the one it saw
    /// ([`AllowanceRecord::check_later_history`]). No message of these roles replaces
    /// another, so a tie never decides.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] naming the rule `later` breaks.
    pub fn check_later_write(&self, later: &AllowanceRecord) -> Result<()> {
        // Records built in code are held to the reader's rules first (Paykit wire 5.4).
        self.check()?;
        later.check()?;
        let kept = |seen: Option<&StoredMessage>, now: Option<&StoredMessage>| {
            seen.is_none_or(|s| now.is_some_and(|n| n.hash() == s.hash()))
        };
        ensure(
            kept(Some(&self.proposal), Some(&later.proposal))
                && kept(self.response.as_ref(), later.response.as_ref())
                && kept(self.end.as_ref(), later.end.as_ref())
                && kept(self.conflicted.as_ref(), later.conflicted.as_ref()),
            "Paykit wire 5.4: a later write drops or replaces a message in a role seen",
        )?;
        self.check_later_history(later)
    }

    /// "Rewrites" (Paykit wire 5.4): an app that has seen this history ignores and flags a
    /// later write whose `grants` doesn't start with it. Writers only append.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] for a later write that changes or drops an entry seen.
    pub fn check_later_history(&self, later: &AllowanceRecord) -> Result<()> {
        ensure(
            self.counterparty == later.counterparty && self.allowance_id == later.allowance_id,
            "Paykit wire 5.4: a later write is another Allowance's record",
        )?;
        ensure(
            later.grants.len() >= self.grants.len()
                && self.grants.iter().zip(&later.grants).all(|(a, b)| a == b),
            "Paykit wire 5.4: a later write doesn't start with the grant history seen",
        )
    }

    /// Write the record value: every member, `null` for absent but `conflicted`, which is left
    /// out when absent, `grants` always, `msg_ids` as its canonical list, [`bounded_msg_ids`] of the stored messages and `msg_ids`, and the
    /// members it doesn't know as it read them (Paykit wire 5, 5.3, 5.4), after the record's
    /// rules, as a reader checks them.
    ///
    /// # Errors
    ///
    /// [`Error::Validation`] for a record that breaks a rule of Paykit wire 5.4, and
    /// [`Error::Invalid`] if the value can't be written.
    pub fn to_json(&self) -> Result<Vec<u8>> {
        self.check().map_err(Error::into_validation)?;
        self.json_unchecked()
    }

    /// Write the record value without its rules, for the vector tests' rejection cases, which
    /// readers must refuse (Paykit wire 5.4). Test-only: no production build has it.
    #[cfg(feature = "test-vectors")]
    #[doc(hidden)]
    pub fn to_json_unchecked(&self) -> Result<Vec<u8>> {
        self.json_unchecked()
    }

    fn json_unchecked(&self) -> Result<Vec<u8>> {
        let stored = std::iter::once(&self.proposal)
            .chain(&self.response)
            .chain(&self.end)
            .chain(&self.conflicted);
        let msg_ids = written_msg_ids(&msg_ids_of(stored), &self.msg_ids)?;
        let out = AllowanceOut {
            v: 1,
            counterparty: pubky_id(&self.counterparty),
            allowance_id: self.allowance_id.as_str(),
            proposal: self.proposal.out(),
            response: self.response.as_ref().map(StoredMessage::out),
            end: self.end.as_ref().map(StoredMessage::out),
            conflicted: self.conflicted.as_ref().map(StoredMessage::out),
            grant: self.grant.as_ref().map(ExecutorGrant::to_json),
            grants: self
                .grants
                .iter()
                .map(|e| {
                    let mut map = Map::new();
                    map.insert("generation".into(), Value::from(e.built_on.generation));
                    map.insert("grant".into(), e.grant.to_json());
                    map.insert("version".into(), Value::from(e.built_on.version));
                    Value::Object(map)
                })
                .collect(),
            msg_ids: &msg_ids,
        };
        let json = serde_json::to_vec(&out)
            .map_err(|_| Error::Invalid(format!("{ALLOWANCES}: can't be written")))?;
        let mut origins: Vec<(String, Origin)> = Vec::new();
        if let Some(grant) = &self.grant {
            origins.push(("/grant".into(), grant.origin));
        }
        for (i, entry) in self.grants.iter().enumerate() {
            origins.push((format!("/grants/{i}"), entry.origin));
            origins.push((format!("/grants/{i}/grant"), entry.grant.origin));
        }
        self.unknown.write_into(json, &ALLOWANCE_SHAPE, &origins)
    }
}

/// The binding rule of an `allowance_ledgers` record (Paykit wire 5.5, Switch spec 8.2):
/// read the ledger at `key`, and act on it only if the key is the one derived from its
/// `counterparty` and `allowance_id` with the reader's naming key, and both equal those of
/// the reader's `allowances` record for that Allowance, against which the ledger's checks
/// then run.
///
/// # Errors
///
/// [`Error::Invalid`]: the reader doesn't act on it and flags it.
pub fn bind_ledger(
    json: &[u8],
    naming_key: &NamingKey,
    key: &[u8; 32],
    allowance: &AllowanceRecord,
) -> Result<Ledger> {
    let ledger = Ledger::from_json(json)?;
    let derived = Collection::AllowanceLedgers.record_key(
        naming_key,
        &natural_key::allowance(&ledger.counterparty, &ledger.allowance_id),
        None,
    )?;
    ensure(
        &derived == key,
        "Paykit wire 5.5: the ledger isn't at the key derived from its counterparty and allowance_id",
    )?;
    ensure(
        ledger.counterparty == allowance.counterparty
            && ledger.allowance_id == allowance.allowance_id,
        "Paykit wire 5.5: the ledger's counterparty or allowance_id isn't its Allowance record's",
    )?;
    Ok(ledger)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A lower-case UUID v4 for `i`.
    fn uuid(i: u64) -> String {
        format!("00000000-0000-4000-8000-{i:012x}")
    }

    #[test]
    fn test_bounded_msg_ids_keeps_stored_and_sixteen_newest() {
        let stored = vec!["s".to_owned()];
        let others: Vec<String> = (0..20).map(|i| format!("o{i}")).collect();
        let kept = bounded_msg_ids(&stored, &others);
        assert_eq!(kept.len(), 1 + RECENT_MSG_IDS);
        assert_eq!(kept[0], "s");
        assert_eq!(kept[1..], others[4..]);
        let few = bounded_msg_ids(&stored, &others[..3]);
        assert_eq!(few.len(), 4);
        // A stored ID among the others is kept once, first.
        let with_stored = bounded_msg_ids(&stored, &["s".to_owned(), "o1".to_owned()]);
        assert_eq!(with_stored, ["s", "o1"]);
    }

    #[test]
    fn test_bounded_msg_ids_dedupes_a_duplicate_heavy_suffix() {
        // 16 distinct IDs, then one ID repeated 20 times: the repeats count once, so the
        // list keeps 16 distinct others, the 15 newest of the first run and the repeated one.
        let mut others: Vec<String> = (0..16).map(|i| format!("o{i}")).collect();
        others.extend(std::iter::repeat_n("x".to_owned(), 20));
        let kept = bounded_msg_ids(&["s".to_owned()], &others);
        let mut want = vec!["s".to_owned()];
        want.extend((1..16).map(|i| format!("o{i}")));
        want.push("x".to_owned());
        assert_eq!(kept, want);
        // Repeats of an older ID don't move it, and never fill the 16.
        let mut mixed: Vec<String> = Vec::new();
        for i in 0..18 {
            mixed.push(format!("o{i}"));
            mixed.push("o0".to_owned());
        }
        let kept = bounded_msg_ids(&[], &mixed);
        assert_eq!(kept.len(), RECENT_MSG_IDS);
        assert_eq!(kept, (2..18).map(|i| format!("o{i}")).collect::<Vec<_>>());
    }

    #[test]
    fn test_msg_ids_read_as_the_canonical_list() {
        let stored = vec![uuid(100)];
        let read = |ids: Vec<String>| {
            let map = serde_json::json!({ "msg_ids": ids })
                .as_object()
                .cloned()
                .unwrap();
            read_msg_ids(&Obj::new(&map, "t"), &stored)
        };
        // More than 16 others: the stored ID, then the 16 newest.
        let many: Vec<String> = (0..20).map(uuid).collect();
        let kept = read(many.clone()).unwrap();
        assert_eq!(kept.len(), 1 + RECENT_MSG_IDS);
        assert_eq!(kept[0], uuid(100));
        assert_eq!(kept[1..], many[4..]);
        // A duplicate-heavy suffix: each ID once, 16 distinct others kept.
        let mut repeats: Vec<String> = (0..16).map(uuid).collect();
        repeats.extend(std::iter::repeat_n(uuid(50), 20));
        let kept = read(repeats).unwrap();
        assert_eq!(kept.len(), 1 + RECENT_MSG_IDS);
        assert_eq!(kept.last(), Some(&uuid(50)));
        assert_eq!(kept[1], uuid(1));
        // Reading the canonical list again gives it back.
        assert_eq!(read(kept.clone()).unwrap(), kept);
        // An ID that isn't a lower-case UUID v4 is still refused.
        assert!(read(vec!["not-a-uuid".to_owned()]).is_err());
    }

    #[test]
    fn test_unrequested_list_version_only_for_a_list() {
        let destination = |source: &str, version: Option<u64>| {
            let mut v = serde_json::json!({ "app_id": "brrz3m6g5rci6iqnzke44q4djc",
                "endpoint_id": "btc-bitcoin-p2wpkh", "payload": "bc1q", "source": source,
                "single_use": false });
            if let Some(n) = version {
                v["list_version"] = serde_json::json!(n);
            }
            v
        };
        read_unrequested(&destination("list", Some(7))).unwrap();
        read_unrequested(&destination("public", None)).unwrap();
        assert!(read_unrequested(&destination("list", None)).is_err());
        assert!(read_unrequested(&destination("public", Some(7))).is_err());
        assert!(read_unrequested(&destination("request", Some(7))).is_err());
    }

    #[test]
    fn test_activity_record_refuses_bad_documents() {
        let own = [1u8; 32];
        assert!(ActivityRecord::read(b"[]", &own, ActivitySide::Payer).is_err());
        assert!(ActivityRecord::read(br#"{"v":2}"#, &own, ActivitySide::Payer).is_err());
        let me = pubky_id(&own);
        let self_record = format!(r#"{{"v":1,"counterparty":"{me}"}}"#);
        assert!(ActivityRecord::read(self_record.as_bytes(), &own, ActivitySide::Payer).is_err());
    }

    #[test]
    fn test_reassociation_condition_ends_at_the_period_start() {
        assert_eq!(
            reassociation_condition(1_772_236_800).unwrap(),
            Condition::ServerTimeIn {
                start: None,
                end: Some(1_772_236_800_000),
            }
        );
        assert!(reassociation_condition(u64::MAX / 1000).is_err());
        // The most conditions of a record.put: a claim_step per listed period, the
        // record_version and that server_time_in.
        assert_eq!(MAX_RECORD_PUT_CONDITIONS, 18);
    }

    #[test]
    fn test_reassociations_shape() {
        let a = AllowanceId::parse("d0efffb7-191b-4709-b6c7-e343af5500b4").unwrap();
        let b = AllowanceId::parse("55de1f16-1a9e-4504-bc9c-57efc3a2b38c").unwrap();
        let entry = |id: &AllowanceId, from_period| Reassociation {
            allowance_id: id.clone(),
            from_period,
        };
        check_reassociations(&[entry(&b, 10), entry(&a, 20)], Some(&a)).unwrap();
        assert!(check_reassociations(&[entry(&a, 10)], Some(&a)).is_err());
        assert!(check_reassociations(&[entry(&b, 10), entry(&b, 20)], Some(&a)).is_err());
        assert!(check_reassociations(&[entry(&b, 20), entry(&a, 10)], Some(&a)).is_err());
        let many: Vec<Reassociation> = (0..17)
            .map(|i| entry(if i % 2 == 0 { &b } else { &a }, i))
            .collect();
        assert!(check_reassociations(&many, Some(&a)).is_err());
        assert!(check_reassociations(&many[..16], Some(&a)).is_ok());
    }

    #[test]
    fn test_unrequested_amount_is_optional() {
        let mut v = serde_json::json!({ "app_id": "brrz3m6g5rci6iqnzke44q4djc",
            "endpoint_id": "btc-bitcoin-p2wpkh", "payload": "bc1q", "source": "public",
            "single_use": false });
        assert_eq!(read_unrequested(&v).unwrap().amount, None);
        v["amount"] = serde_json::json!({ "asset": "btc", "value": "0.0015" });
        let read = read_unrequested(&v).unwrap();
        assert_eq!(
            read.amount.as_ref().map(|a| a.value.as_str()),
            Some("0.0015")
        );
        assert_eq!(unrequested_json(&read), v);
        v["amount"] = serde_json::json!("0.0015");
        assert!(read_unrequested(&v).is_err());
    }

    #[test]
    fn test_unknown_members_by_shape() {
        static INNER: Shape = Shape {
            name: "inner",
            id: &["id"],
            carried: false,
            members: &[("id", Member::Leaf)],
        };
        static ELEMENT: Shape = Shape {
            name: "element",
            id: &[],
            carried: true,
            members: &[("k", Member::Leaf)],
        };
        static OUTER: Shape = Shape {
            name: "outer",
            id: &[],
            carried: false,
            members: &[
                ("items", Member::Array(&INNER)),
                ("carried", Member::Array(&ELEMENT)),
                ("v", Member::Leaf),
            ],
        };
        let read = serde_json::json!({ "v": 1, "x": 2,
            "items": [{ "id": 1, "a": "one" }, { "id": 2, "a": "two" }],
            "carried": [{ "k": 1, "b": "first" }, { "k": 1, "b": "second" }] });
        let (unknown, origins) = UnknownMembers::read(read.as_object().unwrap(), &OUTER);
        let (first, second) = (origins.at("/carried/0"), origins.at("/carried/1"));
        // The items reordered and one dropped: the kept one keeps its member. The carried
        // elements, the first dropped and a new one put before the second: each has its own.
        let written = serde_json::json!({ "v": 1, "items": [{ "id": 3 }, { "id": 2 }],
            "carried": [{ "k": 1 }, { "k": 1 }] });
        let out: Value = serde_json::from_slice(
            &unknown
                .write_into(
                    serde_json::to_vec(&written).unwrap(),
                    &OUTER,
                    &[
                        ("/carried/0".into(), Origin::default()),
                        ("/carried/1".into(), second),
                    ],
                )
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            out,
            serde_json::json!({ "v": 1, "x": 2, "items": [{ "id": 3 }, { "id": 2, "a": "two" }],
                "carried": [{ "k": 1 }, { "k": 1, "b": "second" }] })
        );
        assert_eq!(first, second, "an origin isn't part of the value");
        // None: the bytes as written.
        let none = UnknownMembers::default();
        assert_eq!(
            none.write_into(b"{\"v\":1}".to_vec(), &OUTER, &[]).unwrap(),
            b"{\"v\":1}"
        );
    }

    #[test]
    fn test_attempted_periods_shape() {
        let map = |v: Value| v.as_object().cloned().unwrap();
        let ok = map(serde_json::json!({ "attempted_periods": [1, 2, 3] }));
        assert_eq!(
            read_attempted_periods(&Obj::new(&ok, "t")).unwrap(),
            vec![1, 2, 3]
        );
        for bad in [
            serde_json::json!({ "attempted_periods": [2, 1] }),
            serde_json::json!({ "attempted_periods": [1, 1] }),
            serde_json::json!({ "attempted_periods": (0..17).collect::<Vec<u64>>() }),
        ] {
            let m = map(bad);
            assert!(read_attempted_periods(&Obj::new(&m, "t")).is_err());
        }
    }
}
