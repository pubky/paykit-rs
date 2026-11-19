//! Pending Allowance proposals (Paykit spec 8.3.1, Paykit wire 5.4).

use super::{
    allowance_message, check_v1, ensure, invalid, natural_key, parse_object, pubky_id, AllowanceId,
    Body, Collection, Error, Kind, Member, NamingKey, Obj, Result, Shape, StoredMessage, StoredOut,
    UnknownMembers, STORED,
};
use serde::Serialize;

const PENDING: &str = "Paykit wire 5.4: allowance_proposals";

static SHAPE: Shape = Shape {
    name: "allowance_proposals",
    id: &[],
    carried: false,
    members: &[
        ("v", Member::Leaf),
        ("counterparty", Member::Leaf),
        ("allowance_id", Member::Leaf),
        ("proposal", Member::Object(&STORED)),
        ("conflicted", Member::Object(&STORED)),
    ],
};

/// One ordinary pending proposal and the first different proposal seen under
/// its Allowance ID. Neither message authorizes spending (Paykit wire 5.4).
#[derive(Clone, Debug)]
pub struct PendingAllowanceRecord {
    counterparty: [u8; 32],
    allowance_id: AllowanceId,
    proposal: StoredMessage,
    conflicted: Option<StoredMessage>,
    unknown: UnknownMembers,
}

#[derive(Serialize)]
struct PendingOut<'a> {
    v: u64,
    counterparty: String,
    allowance_id: &'a str,
    proposal: StoredOut,
    #[serde(skip_serializing_if = "Option::is_none")]
    conflicted: Option<StoredOut>,
}

impl PendingAllowanceRecord {
    /// Open a pending record for a signed proposal this identity sent or received.
    ///
    /// # Errors
    ///
    /// The message is another kind or does not join this identity to a counterparty.
    pub fn new(own: &[u8; 32], proposal: StoredMessage) -> Result<Self> {
        let Body::AllowanceProposal(body) = proposal.body() else {
            return invalid("Paykit wire 5.4: a pending record requires an Allowance proposal");
        };
        let sender = proposal.sender();
        let recipient = proposal.message().to;
        let counterparty = if sender == *own {
            recipient
        } else if recipient == *own {
            sender
        } else {
            return invalid(
                "Paykit wire 5.4: a pending proposal must be sent or received by this identity",
            );
        };
        Ok(Self {
            counterparty,
            allowance_id: body.allowance_id.clone(),
            proposal,
            conflicted: None,
            unknown: UnknownMembers::default(),
        })
    }

    /// The counterparty bound by the signed proposal.
    pub fn counterparty(&self) -> &[u8; 32] {
        &self.counterparty
    }

    /// The ID bound by both signed proposals.
    pub fn allowance_id(&self) -> &AllowanceId {
        &self.allowance_id
    }

    /// The original proposal.
    pub fn proposal(&self) -> &StoredMessage {
        &self.proposal
    }

    /// The first distinct proposal for the same Allowance, if held.
    pub fn conflicted(&self) -> Option<&StoredMessage> {
        self.conflicted.as_ref()
    }

    /// Remember a different proposal for the same ID and parties. A repeat
    /// or a proposal after the first conflict leaves the record unchanged.
    ///
    /// # Errors
    ///
    /// The message belongs to another Allowance or is not a proposal.
    pub fn note_proposal(&mut self, other: StoredMessage) -> Result<bool> {
        let original = self.proposal.message();
        let incoming = other.message();
        let same_parties = (incoming.from.identity == original.from.identity
            && incoming.to == original.to)
            || (incoming.from.identity == original.to && incoming.to == original.from.identity);
        ensure(
            same_parties && matches!(other.body(), Body::AllowanceProposal(p) if p.allowance_id == self.allowance_id),
            "Paykit wire 5.4: a pending conflict must be a proposal for the same Allowance and parties",
        )?;
        if self.conflicted.is_some() || other.hash() == self.proposal.hash() {
            return Ok(false);
        }
        self.conflicted = Some(other);
        Ok(true)
    }

    /// Verify the signed value and bind its ID and parties to its unsigned fields.
    ///
    /// # Errors
    ///
    /// A required field, signature, proposal, counterparty or conflict is invalid.
    pub fn read(json: &[u8], own: &[u8; 32]) -> Result<Self> {
        let map = parse_object(json, PENDING)?;
        let obj = Obj::new(&map, PENDING);
        check_v1(&obj)?;
        let counterparty = obj.req_pubky("counterparty")?;
        let allowance_id = AllowanceId::parse(obj.req_str("allowance_id")?)?;
        let message = |value| {
            allowance_message(
                value,
                own,
                &counterparty,
                None,
                &[Kind::AllowanceProposal],
                &allowance_id,
            )
        };
        let mut record = Self::new(own, message(obj.req("proposal")?)?)?;
        if let Some(conflict) = obj.opt("conflicted") {
            ensure(
                record.note_proposal(message(conflict)?)?,
                "Paykit wire 5.4: a pending conflict must differ from the original proposal",
            )?;
        }
        record.unknown = UnknownMembers::read(&map, &SHAPE).0;
        Ok(record)
    }

    /// The key of this ordinary proposal record (Paykit wire 1.5, 5.4).
    ///
    /// # Errors
    ///
    /// The Switch cannot derive the key from the policy.
    pub fn record_key(&self, naming_key: &NamingKey) -> Result<[u8; 32]> {
        Collection::AllowanceProposals.record_key(
            naming_key,
            &natural_key::allowance(&self.counterparty, &self.allowance_id),
            None,
        )
    }

    /// Verify a pending proposal held at this key (Paykit wire 5.4).
    ///
    /// # Errors
    ///
    /// The record does not read or its parties and Allowance ID derive another key.
    pub fn bind(
        json: &[u8],
        own: &[u8; 32],
        naming_key: &NamingKey,
        key: &[u8; 32],
    ) -> Result<Self> {
        let record = Self::read(json, own)?;
        ensure(
            &record.record_key(naming_key)? == key,
            "Paykit wire 5.4: the pending proposal is at another record key",
        )?;
        Ok(record)
    }

    /// Check a later value of this pending record, up to its deletion (Paykit wire 5.4).
    ///
    /// # Errors
    ///
    /// The original proposal changed or the first conflict was dropped or replaced.
    pub fn check_later_write(&self, later: &Self) -> Result<()> {
        ensure(
            self.counterparty == later.counterparty
                && self.proposal.hash() == later.proposal.hash(),
            "Paykit wire 5.4: a later write replaces the pending proposal",
        )?;
        if let Some(conflict) = &self.conflicted {
            ensure(
                later.conflicted.as_ref().map(StoredMessage::hash) == Some(conflict.hash()),
                "Paykit wire 5.4: a later write drops or replaces the pending conflict",
            )?;
        }
        Ok(())
    }

    /// Write the pending proposal and preserve unknown members (Paykit wire 5).
    ///
    /// # Errors
    ///
    /// The record cannot be encoded.
    pub fn to_json(&self) -> Result<Vec<u8>> {
        let out = PendingOut {
            v: 1,
            counterparty: pubky_id(&self.counterparty),
            allowance_id: self.allowance_id.as_str(),
            proposal: self.proposal.out(),
            conflicted: self.conflicted.as_ref().map(StoredMessage::out),
        };
        let bytes = serde_json::to_vec(&out)
            .map_err(|_| Error::Invalid(format!("{PENDING}: can't be written")))?;
        self.unknown.write_into(bytes, &SHAPE, &[])
    }
}
