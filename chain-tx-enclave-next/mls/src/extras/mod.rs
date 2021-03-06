///! This module contains additional parts that are a part of the MLS draft spec,
///! but are required for resolving relevant open issues in the draft spec
///! or for extra conventions / operations: https://github.com/crypto-com/chain-docs/blob/master/docs/modules/tdbe.md.
///!
///! At the moment, one issue is that the node generating Commit/Welcome
///! may put "bogus" in the ciphertext, which will block nodes (newly joining or on the affected
///! path) from obtaining the new group state.
///! The sketched out / unverified solution to that is that the affected member may
///! reveal the shared secret, so that other members can verify that the affected member
///! received a bad update.
///!
///! NOTE: https://mailarchive.ietf.org/arch/msg/mls/DCEKbsnoRKmFTmCuMT-rHIfDapA/
///! one discussed issue is that the attacker may choose the ephemeral pubkey,
///! such that some previous secret value is revealed to him through this "NACK" mechanism.
///! The suggestion is to use Schnorr NIZK proof of the ephemeral pubkey for every ciphertext,
///! but:
///! 1) this is clumsy, as the HPKE setup API doesn't expose the ephemeral secrets.
///! 2) in our case / threat model, it does not seem to matter:
///! - if the attacker can do something like this, it means the attacker managed to breach TEE
///! (unless it's through a bug in the Rust code itself)
///! - if the attacker breached TEE, the "expected" worst case is
///!   "breaking confidentiality" temporarily, i.e. they can read old ledger records
///! -> they don't need to produce tweaked MLS handshakes messages for that,
///!    they could instead "unseal" old TEE-sealed data.
///!
///! the "unexpected" worst case would be breaking ledger integrity (such as through DoS
///! on certain honest nodes / MLS members) which should be prevented
///! by handshake NACK mechanism + BFT consensus.

/// module for external validation
mod validation;

use crate::key::IdentityPublicKey;
use crate::keypackage::Timespec;
use crate::message::MLSPlaintext;
use crate::tree::TreePublicKey;
use crate::tree_math::{LeafSize, NodeSize, ParentSize};
use parity_scale_codec::{Decode, Encode};
use ra_client::AttestedCertVerifier;
use rustls::internal::msgs::codec::Codec;
use secrecy::SecretVec;
use subtle::ConstantTimeEq;
pub use validation::{check_nodejoin, NodeJoinError, NodeJoinResult};
/// module for dleq proofs
mod dleq;

pub use dleq::NackDleqProof;

/// FIXME: official spec may differ
#[derive(Encode, Decode)]
pub struct NackMsgContent {
    /// the sender leaf of NACK -- i.e. affected receiver of Commit
    pub sender: u32,
    /// sha-2 hash of `MLSPlaintext` with Commit
    pub commit_id: [u8; 32],
    /// index of affected encrypted_path_secret
    pub path_secret_index: u32,
    /// proof with the disclosed shared secret
    pub proof: NackDleqProof,
}

/// FIXME: official spec may differ
#[derive(Encode, Decode)]
pub struct NackMsg {
    pub content: NackMsgContent,
    /// ecdsa signature on the NackMsgContent (SCALE-serialized,
    /// as there's no "official" NACK defined for compatibility checking)
    pub signature: Vec<u8>,
}

/// FIXME: official spec may differ
#[derive(Debug)]
pub enum NackError {
    InvalidSender,
    InvalidCommit,
    InvalidPath,
    InvalidProof,
    InvalidSignature,
    /// no problem was found
    ValidPath,
}

/// FIXME: official spec may differ
#[derive(Debug)]
pub enum NackResult {
    /// path secret could not possibly be decrypted by the receiver
    CannotDecrypt,
    /// path secret does not correspond to the public node key
    PathSecretMismatch,
}

impl NackMsg {
    /// verifies Nack message against a previously sent Commit
    pub fn verify(
        &self,
        tree: &TreePublicKey,
        commit: &MLSPlaintext,
        ra_verifier: &impl AttestedCertVerifier,
        now: Timespec,
        encoded_ctx: &[u8],
    ) -> Result<NackResult, NackError> {
        let leaf_len = tree.leaf_len();
        let commit_sender = LeafSize(commit.content.sender.sender);
        let nack_sender = LeafSize(self.content.sender);
        let nack_sender_kp = tree
            .get_package(nack_sender)
            .ok_or(NackError::InvalidSender)?;
        let commit_id = tree.cs.hash(&commit.get_encoding());
        if !bool::from(commit_id.ct_eq(&self.content.commit_id)) {
            return Err(NackError::InvalidCommit);
        }
        let commit_content = commit.get_commit().ok_or(NackError::InvalidCommit)?;
        // only applies to removal/update, as for Add/Welcome, there's no path and for Welcome, the requesting member can request elsewhere if invalid
        let path = commit_content
            .path
            .as_ref()
            .ok_or(NackError::InvalidCommit)?;
        // if there's no ancestor, this would be meaningless -- committer sending NACK for its own commit?
        let ancestor = ParentSize::common_ancestor(commit_sender, nack_sender)
            .ok_or(NackError::InvalidCommit)?;
        // incomplete paths are expected to be checked: https://github.com/crypto-com/chain-docs/issues/190 https://github.com/crypto-com/chain-docs/issues/189
        let path_node = NodeSize::from(commit_sender)
            .direct_path(leaf_len)
            .into_iter()
            .zip(path.nodes.iter())
            .find(|(n, _)| *n == ancestor)
            .ok_or(NackError::InvalidCommit)?
            .1;

        let affected_path_secret = path_node
            .encrypted_path_secret
            .get(self.content.path_secret_index as usize)
            .ok_or(NackError::InvalidCommit)?;
        let sender_node_index = NodeSize::from(nack_sender);
        let path_indices = tree.resolve(sender_node_index);
        let node_index = path_indices
            .get(self.content.path_secret_index as usize)
            .ok_or(NackError::InvalidPath)?;
        // one won't encrypt to blank nodes
        let node_key = tree.nodes[node_index.node_index()]
            .public_key()
            .ok_or(NackError::InvalidPath)?;
        self.content
            .proof
            .verify(&node_key, &affected_path_secret.kem_output)
            .map_err(|_| NackError::InvalidProof)?;
        let nack_sender_id = nack_sender_kp
            .verify(ra_verifier, now)
            .map_err(|_| NackError::InvalidSignature)?
            .public_key;
        let public_key = IdentityPublicKey::new_unsafe(nack_sender_id.to_vec());
        public_key
            .verify_signature(&self.content.encode(), &self.signature)
            .map_err(|_| NackError::InvalidSignature)?;
        let overlap_path_secret =
            self.content
                .proof
                .decrypt_after_proof(&node_key, &affected_path_secret, encoded_ctx);
        if let Ok(path_secret) = overlap_path_secret {
            let overlap_path_secret = SecretVec::new(path_secret);
            let direct_path = NodeSize::from(nack_sender).direct_path(leaf_len);
            let overlap_pos = direct_path
                .iter()
                .position(|&p| p == ancestor)
                .expect("overlap is supposed to be ancestor");
            let overlap_path = &direct_path[overlap_pos + 1..];

            // the path secrets above(not including) the overlap node
            let mut secrets = vec![];
            for _ in overlap_path.iter() {
                secrets.push(
                    tree.cs
                        .expand_label(
                            secrets.last().unwrap_or(&overlap_path_secret),
                            vec![],
                            "path",
                            &[],
                            tree.cs.secret_size(),
                        )
                        .expect("expand label works"),
                );
            }

            // verify the new path secrets match public keys
            if tree
                .verify_node_private_key(&overlap_path_secret, ancestor)
                .is_err()
            {
                return Ok(NackResult::PathSecretMismatch);
            }
            for (secret, &parent) in secrets.iter().skip(1).zip(overlap_path) {
                if tree.verify_node_private_key(secret, parent).is_err() {
                    return Ok(NackResult::PathSecretMismatch);
                }
            }
            Err(NackError::ValidPath)
        } else {
            Ok(NackResult::CannotDecrypt)
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::group::test::{get_fake_keypackage, three_member_setup, MockVerifier};
    use crate::group::GroupAux;
    use crate::message::{ContentType, MLSPlaintextTBS};

    fn corrupt_and_sign_commit(
        commit: &MLSPlaintext,
        sender: &GroupAux,
        valid_ciphertext: bool,
    ) -> MLSPlaintext {
        let mut new_commit = commit.clone();
        let (mut new_commit_content, confirmation) = match &new_commit.content.content {
            ContentType::Commit {
                commit,
                confirmation,
            } => (commit.clone(), confirmation.clone()),
            _ => unreachable!(),
        };
        let mut path = new_commit_content.path.clone().expect("path");
        if valid_ciphertext {
            let init_key = sender.tree.nodes[0]
                .public_key()
                .expect("not blank node TODO");
            path.nodes[0].encrypted_path_secret[0] = sender
                .tree
                .cs
                .encrypt(vec![0u8; 32], &init_key, &sender.context.get_encoding())
                .expect("encrypt")
        } else {
            path.nodes[0].encrypted_path_secret[0].ciphertext[0] = 0;
        }

        new_commit_content.path = Some(path);
        new_commit.content.content = ContentType::Commit {
            commit: new_commit_content,
            confirmation,
        };
        let to_be_signed = MLSPlaintextTBS {
            context: sender.context.clone(),
            content: new_commit.content.clone(),
        }
        .get_encoding();
        let signature = sender
            .kp_secret
            .credential_private_key
            .sign(&to_be_signed)
            .unwrap();
        new_commit.signature = signature;

        new_commit
    }

    #[test]
    fn test_nack_verify_fail_valid() {
        // FIXME: test other errors
        let ra_verifier = MockVerifier {};
        let (mut member1_group, mut member2_group, mut member3_group) = three_member_setup();

        let (member2, member2_secret) = get_fake_keypackage();
        let proposals = vec![member2_group
            .get_signed_self_update(member2.clone(), member2_secret)
            .unwrap()];
        let (commit, _welcome) = member2_group.commit_proposals(&proposals).unwrap();
        let path = commit
            .get_commit()
            .expect("commit")
            .path
            .as_ref()
            .expect("path");
        let proof = NackDleqProof::get_nack_dleq_proof(
            &member1_group.kp_secret.init_private_key,
            &path.nodes[0].encrypted_path_secret[0].kem_output,
        )
        .expect("proof");
        let mut commit_id = [0u8; 32];

        commit_id.copy_from_slice(&member1_group.tree.cs.hash(&commit.get_encoding()));
        let nack_content = NackMsgContent {
            sender: 0,
            commit_id,
            path_secret_index: 0,
            proof,
        };
        let nack_signature = member1_group
            .kp_secret
            .credential_private_key
            .sign(&nack_content.encode())
            .unwrap();
        let nack = NackMsg {
            content: nack_content,
            signature: nack_signature,
        };
        member1_group
            .process_commit(commit.clone(), &proposals, &ra_verifier, 0)
            .expect("commit ok");
        let ctx = member3_group.context.get_encoding();
        member3_group
            .process_commit(commit.clone(), &proposals, &ra_verifier, 0)
            .expect("commit ok");
        assert!(matches!(
            nack.verify(&member3_group.tree, &commit, &ra_verifier, 0, &ctx),
            Err(NackError::ValidPath)
        ));
    }

    #[test]
    fn test_nack_verify_decrypt_fail() {
        let ra_verifier = MockVerifier {};
        let (mut member1_group, mut member2_group, mut member3_group) = three_member_setup();

        // member 1 -- affected
        // member 2 -- malicious / unaffected
        // member 3 -- honest / unaffected (verifying member1 claim)
        let (member2, member2_secret) = get_fake_keypackage();
        let proposals = vec![member2_group
            .get_signed_self_update(member2.clone(), member2_secret)
            .unwrap()];
        let (commit, _welcome) = member2_group.commit_proposals(&proposals).unwrap();
        // case 1: decryption fails
        let commit = corrupt_and_sign_commit(&commit, &member2_group, false);

        // FIXME: the error shouldn't be discovered in "process commit", but some "verify" commit
        // + many things in Commit should be verified -- e.g. that "kem_output" is a valid pubkey
        member1_group
            .process_commit(commit.clone(), &proposals, &ra_verifier, 0)
            .expect_err("commit not ok");
        let ctx = member3_group.context.get_encoding();
        // for group 3, it should look ok
        member3_group
            .process_commit(commit.clone(), &proposals, &ra_verifier, 0)
            .expect("commit ok");
        let path = commit
            .get_commit()
            .expect("commit")
            .path
            .as_ref()
            .expect("path");
        let proof = NackDleqProof::get_nack_dleq_proof(
            &member1_group.kp_secret.init_private_key,
            &path.nodes[0].encrypted_path_secret[0].kem_output,
        )
        .expect("proof");
        let mut commit_id = [0u8; 32];

        commit_id.copy_from_slice(&member1_group.tree.cs.hash(&commit.get_encoding()));
        let nack_content = NackMsgContent {
            sender: 0,
            commit_id,
            path_secret_index: 0,
            proof,
        };
        let nack_signature = member1_group
            .kp_secret
            .credential_private_key
            .sign(&nack_content.encode())
            .unwrap();
        let nack = NackMsg {
            content: nack_content,
            signature: nack_signature,
        };
        assert!(matches!(
            nack.verify(&member3_group.tree, &commit, &ra_verifier, 0, &ctx),
            Ok(NackResult::CannotDecrypt)
        ));
    }

    #[test]
    fn test_nack_verify_path_fail() {
        let ra_verifier = MockVerifier {};
        let (mut member1_group, mut member2_group, mut member3_group) = three_member_setup();

        // member 1 -- affected
        // member 2 -- malicious / unaffected
        // member 3 -- honest / unaffected (verifying member1 claim)
        let (member2, member2_secret) = get_fake_keypackage();
        let proposals = vec![member2_group
            .get_signed_self_update(member2.clone(), member2_secret)
            .unwrap()];
        let (commit, _welcome) = member2_group.commit_proposals(&proposals).unwrap();

        // case 2: path secret doesn't match
        let commit = corrupt_and_sign_commit(&commit, &member2_group, true);

        // FIXME: the error shouldn't be discovered in "process commit", but some "verify" commit
        // + many things in Commit should be verified -- e.g. that "kem_output" is a valid pubkey
        member1_group
            .process_commit(commit.clone(), &proposals, &ra_verifier, 0)
            .expect_err("commit not ok");
        let ctx = member3_group.context.get_encoding();
        // for group 3, it should look ok
        member3_group
            .process_commit(commit.clone(), &proposals, &ra_verifier, 0)
            .expect("commit ok");

        let path = commit
            .get_commit()
            .expect("commit")
            .path
            .as_ref()
            .expect("path");
        let proof = NackDleqProof::get_nack_dleq_proof(
            &member1_group.kp_secret.init_private_key,
            &path.nodes[0].encrypted_path_secret[0].kem_output,
        )
        .expect("proof");
        let mut commit_id = [0u8; 32];

        commit_id.copy_from_slice(&member1_group.tree.cs.hash(&commit.get_encoding()));
        let nack_content = NackMsgContent {
            sender: 0,
            commit_id,
            path_secret_index: 0,
            proof,
        };
        let nack_signature = member1_group
            .kp_secret
            .credential_private_key
            .sign(&nack_content.encode())
            .unwrap();
        let nack = NackMsg {
            content: nack_content,
            signature: nack_signature,
        };
        assert!(matches!(
            nack.verify(&member3_group.tree, &commit, &ra_verifier, 0, &ctx),
            Ok(NackResult::PathSecretMismatch)
        ));
    }
}
