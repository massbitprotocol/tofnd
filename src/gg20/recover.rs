//! This module handles the recover gRPC.
//! Request includes [proto::message_in::Data::KeygenInit] struct and encrypted recovery info.
//! The recovery info is decrypted by party's mnemonic seed and saved in the KvStore.

use super::{keygen::types::KeygenInitSanitized, proto, service::Gg20Service, types::PartyInfo};
use crate::TofndError;
use tofn::{
    collections::TypedUsize,
    gg20::keygen::{
        recover_party_keypair, recover_party_keypair_unsafe, KeyShareRecoveryInfo, KeygenPartyId,
        KeygenPartyShareCounts, PartyKeyPair, SecretKeyShare, SecretRecoveryKey,
    },
    sdk::api::PartyShareCounts,
};

impl Gg20Service {
    pub(super) async fn handle_recover(
        &mut self,
        request: proto::RecoverRequest,
    ) -> Result<(), TofndError> {
        // get keygen init sanitized from request
        let keygen_init_sanitized = {
            let keygen_init = match request.keygen_init {
                Some(keygen_init) => keygen_init,
                None => return Err(From::from("missing keygen_init field in recovery request")),
            };
            Self::keygen_sanitize_args(keygen_init)?
        };

        // recover secret key shares from request
        // get mnemonic seed
        let secret_recovery_key = self.seed().await?;
        let secret_key_shares = self
            .recover_secret_key_shares(
                &secret_recovery_key,
                &request.share_recovery_infos,
                keygen_init_sanitized.my_index,
                keygen_init_sanitized.new_key_uid.as_bytes(),
                &keygen_init_sanitized.party_share_counts,
                keygen_init_sanitized.threshold,
            )
            .map_err(|err| format!("Failed to acquire secret key share {}", err))?;

        Ok(self
            .update_share_kv_store(keygen_init_sanitized, secret_key_shares)
            .await?)
    }

    // allow for users to select whether to use big primes or not
    #[allow(clippy::too_many_arguments)]
    fn recover(
        &self,
        party_keypair: &PartyKeyPair,
        recovery_infos: &[KeyShareRecoveryInfo],
        party_id: TypedUsize<KeygenPartyId>,
        subshare_id: usize, // in 0..party_share_counts[party_id]
        party_share_counts: KeygenPartyShareCounts,
        threshold: usize,
    ) -> Result<SecretKeyShare, TofndError> {
        let recover = SecretKeyShare::recover(
            party_keypair,
            recovery_infos,
            party_id,
            subshare_id,
            party_share_counts,
            threshold,
        );

        // map error and return result
        recover.map_err(|_| {
            From::from(format!(
                "Cannot recover share [{}] of party [{}]",
                subshare_id, party_id,
            ))
        })
    }

    /// get recovered secret key shares from serilized share recovery info
    fn recover_secret_key_shares(
        &self,
        secret_recovery_key: &SecretRecoveryKey,
        serialized_share_recovery_infos: &[Vec<u8>],
        my_tofnd_index: usize,
        session_nonce: &[u8],
        party_share_counts: &[usize],
        threshold: usize,
    ) -> Result<Vec<SecretKeyShare>, TofndError> {
        // gather deserialized share recovery infos. Avoid using map() because deserialization returns Result
        let mut deserialized_share_recovery_infos =
            Vec::with_capacity(serialized_share_recovery_infos.len());
        for bytes in serialized_share_recovery_infos {
            deserialized_share_recovery_infos.push(bincode::deserialize(bytes)?);
        }

        // get my share count safely
        let my_share_count = party_share_counts.get(my_tofnd_index).ok_or(format!(
            "index {} is out of party_share_counts bounds {}",
            my_tofnd_index,
            party_share_counts.len()
        ))?;

        let party_share_counts = PartyShareCounts::from_vec(party_share_counts.to_owned())
            .map_err(|_| format!("PartyCounts::from_vec() error for {:?}", party_share_counts))?;

        let party_keypair = match self.safe_keygen {
            true => recover_party_keypair(secret_recovery_key, session_nonce),
            false => recover_party_keypair_unsafe(secret_recovery_key, session_nonce),
        }
        .map_err(|_| "party keypair recovery failed".to_string())?;

        // gather secret key shares from recovery infos
        let mut secret_key_shares = Vec::with_capacity(*my_share_count);
        for i in 0..*my_share_count {
            secret_key_shares.push(self.recover(
                &party_keypair,
                &deserialized_share_recovery_infos,
                TypedUsize::from_usize(my_tofnd_index),
                i,
                party_share_counts.clone(),
                threshold,
            )?);
        }

        Ok(secret_key_shares)
    }

    /// attempt to write recovered secret key shares to the kv-store
    async fn update_share_kv_store(
        &mut self,
        keygen_init_sanitized: KeygenInitSanitized,
        secret_key_shares: Vec<SecretKeyShare>,
    ) -> Result<(), TofndError> {
        // try to make a reservation
        let reservation = self
            .shares_kv
            .reserve_key(keygen_init_sanitized.new_key_uid)
            .await?;
        // acquire kv-data
        let kv_data = PartyInfo::get_party_info(
            secret_key_shares,
            keygen_init_sanitized.party_uids,
            keygen_init_sanitized.party_share_counts,
            keygen_init_sanitized.my_index,
        );
        // try writing the data to the kv-store
        Ok(self.shares_kv.put(reservation, kv_data).await?)
    }
}
