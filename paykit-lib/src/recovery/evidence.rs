//! The disclosed inputs to a recovery report (Paykit wire 5.10).

use std::collections::BTreeSet;

use pubky_dispatch_core::{
    chain::{SignedHead, TransitionProof},
    enc::{hash, lp},
    ops::SignedOp,
    records::{AppCert, IdentityDoc},
};

use super::RecoveryCommitments;
use crate::error::{ensure, invalid, Result};

/// A source of evidence. Signed objects keep their original body bytes.
pub enum RecoveryWitness<'a> {
    /// An identity-signed directory.
    Identity(&'a IdentityDoc),
    /// An identity-signed app certificate.
    Certificate(&'a AppCert),
    /// An app's signed operation.
    Operation(&'a SignedOp),
    /// A service's signed transition proof.
    Proof(&'a TransitionProof),
    /// A signed service chain head.
    Head(&'a SignedHead),
    /// Exact bytes kept at a locator in a named, versioned local format.
    Local {
        /// The format name and positive decimal version, separated by a slash.
        scheme: &'a str,
        /// A nonempty locator in this format.
        key: &'a [u8],
        /// The exact bytes the original kept.
        value: &'a [u8],
    },
}

/// What a recovery inventory entry concerns.
#[derive(Clone, Copy)]
pub enum RecoverySubject<'a> {
    /// A Dispatch shared record key.
    Record([u8; 32]),
    /// A Paykit obligation's claim key.
    Claim([u8; 32]),
    /// A full settlement tag, before it is shortened for the mark.
    Settlement([u8; 32]),
    /// A protective duty known by its local format and locator.
    Local {
        /// The format name and positive decimal version, separated by a slash.
        scheme: &'a str,
        /// A nonempty locator in this format.
        key: &'a [u8],
    },
}

/// The witnesses the original retained for one inventory subject.
pub struct RecoveryItem<'a> {
    /// The subject, unique within this inventory.
    pub subject: RecoverySubject<'a>,
    /// All disclosed witnesses for this subject.
    pub witnesses: &'a [RecoveryWitness<'a>],
}

/// The original's disposition for an inventory subject.
#[derive(Clone, Copy)]
pub enum RecoveryState {
    /// The work remains unresolved.
    Held,
    /// The original found the evidence represented at the recovered service.
    Represented,
    /// Another holder accepted protective custody.
    Transferred,
    /// The original has evidence of a terminal outcome.
    Terminal,
}

/// One original's decision and its basis. Consumers check the basis at use.
pub struct RecoveryDecision<'a> {
    /// The inventory subject this decision names.
    pub subject: RecoverySubject<'a>,
    /// The state declared by the original.
    pub state: RecoveryState,
    /// Its supporting witnesses, required for every state except a hold.
    pub witnesses: &'a [RecoveryWitness<'a>],
}

fn frame(parts: &[&[u8]]) -> Result<Vec<u8>> {
    ensure(
        parts.iter().all(|part| u32::try_from(part.len()).is_ok()),
        "Paykit wire 5.10: evidence field exceeds the length prefix",
    )?;
    Ok(lp(parts))
}

fn local_locator(scheme: &str, key: &[u8]) -> Result<()> {
    let Some((name, version)) = scheme.split_once('/') else {
        return invalid("Paykit wire 5.10: local evidence needs a versioned scheme");
    };
    ensure(
        !key.is_empty()
            && !name.is_empty()
            && name
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'.' | b'-'))
            && !version.is_empty()
            && !version.starts_with('0')
            && version.bytes().all(|c| c.is_ascii_digit()),
        "Paykit wire 5.10: invalid local evidence scheme or key",
    )
}

impl RecoveryWitness<'_> {
    fn bytes(&self) -> Result<Vec<u8>> {
        let (tag, doc): (&[u8], _) = match self {
            Self::Identity(doc) => (b"identity", doc.signed()),
            Self::Certificate(cert) => (b"certificate", cert.signed()),
            Self::Operation(op) => (b"operation", op.signed()),
            Self::Proof(proof) => (b"proof", proof.signed()),
            Self::Head(head) => {
                return frame(&[
                    b"head",
                    &head.chain_version.to_be_bytes(),
                    &head.generation.to_be_bytes(),
                    &head.seq.to_be_bytes(),
                    &head.chain_hash,
                    &head.sig,
                ])
            }
            Self::Local { scheme, key, value } => {
                local_locator(scheme, key)?;
                return frame(&[b"local", scheme.as_bytes(), key, value]);
            }
        };
        frame(&[tag, &doc.doc_bytes()?])
    }
}

impl RecoverySubject<'_> {
    fn bytes(self) -> Result<Vec<u8>> {
        match self {
            Self::Record(key) => frame(&[b"record", &key]),
            Self::Claim(key) => frame(&[b"claim", &key]),
            Self::Settlement(key) => frame(&[b"settlement", &key]),
            Self::Local { scheme, key } => {
                local_locator(scheme, key)?;
                frame(&[b"local", scheme.as_bytes(), key])
            }
        }
    }
}

fn set(mut rows: Vec<Vec<u8>>) -> Result<Vec<u8>> {
    rows.sort_unstable();
    ensure(
        !rows.windows(2).any(|pair| pair[0] == pair[1]),
        "Paykit wire 5.10: duplicate evidence row",
    )?;
    frame(&rows.iter().map(Vec::as_slice).collect::<Vec<_>>())
}

fn witnesses(rows: &[RecoveryWitness<'_>]) -> Result<Vec<u8>> {
    set(rows
        .iter()
        .map(RecoveryWitness::bytes)
        .collect::<Result<_>>()?)
}

impl RecoveryCommitments {
    /// Recompute the three digests from disclosed evidence, in any input order.
    /// Matching the signed digests identifies the original's evidence. Its authority,
    /// completeness and the claimed outcome need their own checks.
    ///
    /// # Errors
    ///
    /// An anchor, locator or set is invalid, or decisions do not cover the subjects exactly.
    pub fn from_evidence(
        anchors: &[RecoveryWitness<'_>],
        items: &[RecoveryItem<'_>],
        decisions: &[RecoveryDecision<'_>],
    ) -> Result<Self> {
        ensure(
            !anchors.is_empty()
                && anchors.iter().all(|anchor| {
                    matches!(
                        anchor,
                        RecoveryWitness::Identity(_)
                            | RecoveryWitness::Head(_)
                            | RecoveryWitness::Local { .. }
                    )
                }),
            "Paykit wire 5.10: invalid recovery cutoff",
        )?;
        let cutoff = hash(&frame(&[
            b"paykit/v1/recovery-cutoff",
            &witnesses(anchors)?,
        ])?);
        let mut subjects = BTreeSet::new();
        let mut rows = Vec::with_capacity(items.len());
        for item in items {
            let subject = item.subject.bytes()?;
            rows.push(frame(&[&subject, &witnesses(item.witnesses)?])?);
            ensure(
                subjects.insert(subject),
                "Paykit wire 5.10: duplicate inventory subject",
            )?;
        }
        let inventory = hash(&frame(&[
            b"paykit/v1/recovery-inventory",
            &cutoff,
            &set(rows)?,
        ])?);
        let mut rows = Vec::with_capacity(decisions.len());
        for decision in decisions {
            let subject = decision.subject.bytes()?;
            ensure(
                subjects.remove(&subject),
                "Paykit wire 5.10: unknown or repeated decision subject",
            )?;
            ensure(
                matches!(decision.state, RecoveryState::Held) || !decision.witnesses.is_empty(),
                "Paykit wire 5.10: missing disposition evidence",
            )?;
            let state: &[u8] = match decision.state {
                RecoveryState::Held => b"held",
                RecoveryState::Represented => b"represented",
                RecoveryState::Transferred => b"transferred",
                RecoveryState::Terminal => b"terminal",
            };
            rows.push(frame(&[&subject, state, &witnesses(decision.witnesses)?])?);
        }
        ensure(
            subjects.is_empty(),
            "Paykit wire 5.10: missing inventory disposition",
        )?;
        let dispositions = hash(&frame(&[
            b"paykit/v1/recovery-dispositions",
            &inventory,
            &set(rows)?,
        ])?);
        Ok(Self {
            cutoff,
            inventory,
            dispositions,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pubky_dispatch_core::enc::b64;

    #[test]
    fn test_recovery_evidence_matches_the_wire_examples_in_either_order() {
        let scheme = "org.example.wallet/1";
        let anchors = [RecoveryWitness::Local {
            scheme,
            key: b"cursor",
            value: b"7",
        }];
        let retained = [RecoveryWitness::Local {
            scheme,
            key: b"receipt",
            value: b"retained",
        }];
        let subject = RecoverySubject::Local { scheme, key: b"q" };
        let claim = RecoverySubject::Claim(std::array::from_fn(|i| i as u8));
        let mut items = [
            RecoveryItem {
                subject,
                witnesses: &retained,
            },
            RecoveryItem {
                subject: claim,
                witnesses: &[],
            },
        ];
        let mut decisions = [
            RecoveryDecision {
                subject,
                state: RecoveryState::Held,
                witnesses: &retained,
            },
            RecoveryDecision {
                subject: claim,
                state: RecoveryState::Held,
                witnesses: &[],
            },
        ];
        let empty = RecoveryCommitments::from_evidence(&anchors, &[], &[]).unwrap();
        assert_eq!(
            b64(&empty.cutoff),
            "rhCdN3XihSPGGx33cP1F80L_vIzI3OWhMfyEyufKcY0"
        );
        assert_eq!(
            b64(&empty.inventory),
            "Nu9zxanD-C_J8ymzxJ9r8ImS3e83jcgp_LYX8QUbv2I"
        );
        assert_eq!(
            b64(&empty.dispositions),
            "WwrY75N40n4BdO50TdyZih3vz_xfkWGn53GWT4hvEXU"
        );
        let full = RecoveryCommitments::from_evidence(&anchors, &items, &decisions).unwrap();
        assert_eq!(
            b64(&full.inventory),
            "B2QDf1Ye_fXOrb0ewKmGudAX1DjbAcFXwz83FNKZ1Bk"
        );
        assert_eq!(
            b64(&full.dispositions),
            "1_2yHdoLyWuVV-LlQ3xWlpj6sPb7_6TKqF_JE40juBo"
        );
        items.reverse();
        decisions.reverse();
        assert_eq!(
            RecoveryCommitments::from_evidence(&anchors, &items, &decisions).unwrap(),
            full
        );
        assert!(RecoveryCommitments::from_evidence(&anchors, &items, &decisions[..1]).is_err());
        decisions[1].subject = decisions[0].subject;
        assert!(RecoveryCommitments::from_evidence(&anchors, &items, &decisions).is_err());
        items[1].subject = items[0].subject;
        assert!(RecoveryCommitments::from_evidence(&anchors, &items, &decisions).is_err());
    }

    #[test]
    fn test_recovery_evidence_rejects_invalid_cutoffs_locators_and_unsupported_decisions() {
        let anchor = || RecoveryWitness::Local {
            scheme: "org.example.wallet/1",
            key: b"scan",
            value: b"",
        };
        assert!(RecoveryCommitments::from_evidence(&[], &[], &[]).is_err());
        assert!(RecoveryCommitments::from_evidence(&[anchor(), anchor()], &[], &[]).is_err());
        for scheme in [
            "",
            "example",
            "/1",
            "example/0",
            "example/01",
            "Example/1",
            "example/1/2",
        ] {
            let malformed = RecoveryWitness::Local {
                scheme,
                key: b"a",
                value: b"",
            };
            assert!(RecoveryCommitments::from_evidence(&[malformed], &[], &[]).is_err());
        }
        let subject = RecoverySubject::Settlement([42; 32]);
        let items = [RecoveryItem {
            subject,
            witnesses: &[],
        }];
        for state in [
            RecoveryState::Represented,
            RecoveryState::Transferred,
            RecoveryState::Terminal,
        ] {
            let decision = RecoveryDecision {
                subject,
                state,
                witnesses: &[],
            };
            assert!(RecoveryCommitments::from_evidence(&[anchor()], &items, &[decision]).is_err());
            let valid = RecoveryDecision {
                subject,
                state,
                witnesses: &[anchor()],
            };
            assert!(RecoveryCommitments::from_evidence(&[anchor()], &items, &[valid]).is_ok());
        }
    }
}
