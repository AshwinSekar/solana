use {
    crate::{
        bls_sigverifier::{BAN_TIMEOUT, NUM_SLOTS_FOR_VERIFY},
        errors::SigVerifyCertError,
        sig_verified_messages::SigVerifiedBatch,
        stats::SigVerifyCertStats,
        utils::send_certs_to_pool,
    },
    agave_bls_cert_verify::cert_verify::Error as BlsCertVerifyError,
    agave_votor_messages::{
        certificate::{Certificate, CertificateType},
        unverified_vote_message::UnverifiedCertificate,
    },
    crossbeam_channel::Sender,
    log::info,
    rayon::{
        ThreadPool,
        iter::{IntoParallelIterator, ParallelIterator},
    },
    solana_clock::Slot,
    solana_measure::measure::Measure,
    solana_pubkey::Pubkey,
    solana_runtime::bank::Bank,
    solana_streamer::nonblocking::simple_qos::SimpleQosBanlist,
    std::collections::{HashMap, HashSet},
    thiserror::Error,
};

pub(super) struct CertPayload {
    pub(super) cert: UnverifiedCertificate,
    pub(super) sender_identity_pubkey: Pubkey,
}

#[derive(Debug, Error)]
enum CertVerifyError {
    #[error("Cert Verification Error {0}")]
    CertVerifyFailed(#[from] BlsCertVerifyError),
    #[error("discarding cert with slot {cert_slot} too far in future from root slot {root_slot}")]
    TooFarInFuture { cert_slot: Slot, root_slot: Slot },
}

struct CertVerifyOutcome {
    verified_cert: Option<Certificate>,
    failures: Vec<(CertVerifyError, Pubkey)>,
    verified_count: usize,
    skipped_after_success: usize,
}

/// Verifies certificates and sends the verified certificates to the consensus pool.
///
/// Additionally inserts valid [`CertificateType`]s into `verified_certs_set`.
/// Any certificate that fails verification will have its sender banlisted.
///
/// Function expects that the caller has already filtered any certs that appear in the
/// [`verified_certs_set`]. Certs with the same [`CertificateType`] in this batch are grouped and
/// verified until the first valid candidate is found.
pub(super) fn verify_and_send_certificates(
    verified_certs_set: &mut HashSet<CertificateType>,
    certs: Vec<CertPayload>,
    root_bank: &Bank,
    channel_to_pool: &Sender<SigVerifiedBatch>,
    banlist: &SimpleQosBanlist,
    thread_pool: &ThreadPool,
) -> Result<SigVerifyCertStats, SigVerifyCertError> {
    for cert in certs.iter().map(|cert_payload| &cert_payload.cert) {
        debug_assert!(!verified_certs_set.contains(&cert.cert_type));
    }
    let mut measure = Measure::start("verify_and_send_certificates");
    let mut stats = SigVerifyCertStats::default();

    if certs.is_empty() {
        return Ok(stats);
    }

    let messages = verify_certs(
        certs,
        root_bank,
        verified_certs_set,
        &mut stats,
        banlist,
        thread_pool,
    );
    stats.sig_verified_certs += messages.len() as u64;
    send_certs_to_pool(messages, channel_to_pool, &mut stats)?;

    measure.stop();
    stats
        .fn_verify_and_send_certs_stats
        .add_sample(measure.as_us());
    Ok(stats)
}

/// Verifies certificates in `certs`, stores a local copy, and prepares them for forwarding.
///
/// The valid certs are inserted into the [`verified_certs_set`].
/// Invalid cert senders are banlisted.
/// Returns a [`SigVerifiedBatch`] constructed from the valid certs.
fn verify_certs(
    certs: Vec<CertPayload>,
    root_bank: &Bank,
    verified_certs_set: &mut HashSet<CertificateType>,
    stats: &mut SigVerifyCertStats,
    banlist: &SimpleQosBanlist,
    thread_pool: &ThreadPool,
) -> SigVerifiedBatch {
    let cert_groups = group_certs_by_type(certs);
    let verified = thread_pool.install(|| {
        cert_groups
            .into_par_iter()
            .map(|certs| verify_cert_group(certs, root_bank))
            .collect::<Vec<_>>()
    });

    let mut certs = Vec::new();
    for outcome in verified {
        stats.certs_to_sig_verify += outcome.verified_count as u64;
        stats.redundant_certs_skipped += outcome.skipped_after_success as u64;

        for (err, sender_identity_pubkey) in outcome.failures {
            handle_cert_verify_error(err, sender_identity_pubkey, stats, banlist);
        }

        if let Some(cert) = outcome.verified_cert {
            if verified_certs_set.insert(cert.cert_type) {
                certs.push(cert);
            } else {
                stats.unnecessary_certs_verified += 1;
            }
        }
    }

    SigVerifiedBatch::Certificates(certs)
}

fn group_certs_by_type(certs: Vec<CertPayload>) -> Vec<Vec<CertPayload>> {
    let mut group_indices = HashMap::<CertificateType, usize>::new();
    let mut groups = Vec::<Vec<CertPayload>>::new();

    for cert_payload in certs {
        let cert_type = cert_payload.cert.cert_type;

        if let Some(&index) = group_indices.get(&cert_type) {
            groups[index].push(cert_payload);
        } else {
            let index = groups.len();
            group_indices.insert(cert_type, index);
            groups.push(vec![cert_payload]);
        }
    }

    groups
}

fn verify_cert_group(certs: Vec<CertPayload>, root_bank: &Bank) -> CertVerifyOutcome {
    let num_certs = certs.len();
    let mut failures = Vec::new();

    for (index, cert_payload) in certs.into_iter().enumerate() {
        match verify_cert(cert_payload.cert, root_bank) {
            Ok(cert) => {
                return CertVerifyOutcome {
                    verified_cert: Some(cert),
                    failures,
                    verified_count: index.saturating_add(1),
                    skipped_after_success: num_certs.saturating_sub(index.saturating_add(1)),
                };
            }
            Err(err) => failures.push((err, cert_payload.sender_identity_pubkey)),
        }
    }

    CertVerifyOutcome {
        verified_cert: None,
        failures,
        verified_count: num_certs,
        skipped_after_success: 0,
    }
}

fn handle_cert_verify_error(
    err: CertVerifyError,
    sender_identity_pubkey: Pubkey,
    stats: &mut SigVerifyCertStats,
    banlist: &SimpleQosBanlist,
) {
    match &err {
        CertVerifyError::CertVerifyFailed(_) => {
            stats.banning_validator += 1;
            if banlist.ban(sender_identity_pubkey, BAN_TIMEOUT) {
                stats.already_banned += 1;
            } else {
                info!(
                    "bls_cert_sigverify: banned sender={sender_identity_pubkey} due to error {err}"
                );
            }
        }
        CertVerifyError::TooFarInFuture { .. } => {}
    }

    match err {
        CertVerifyError::CertVerifyFailed(_) => {
            stats.certificate_verification_failed += 1;
        }
        CertVerifyError::TooFarInFuture { .. } => {
            stats.too_far_in_future += 1;
        }
    }
}

fn verify_cert(
    cert: UnverifiedCertificate,
    root_bank: &Bank,
) -> Result<Certificate, CertVerifyError> {
    let cert_slot = cert.cert_type.slot();
    let root_slot = root_bank.slot();
    if cert_slot > root_slot.saturating_add(NUM_SLOTS_FOR_VERIFY) {
        return Err(CertVerifyError::TooFarInFuture {
            cert_slot,
            root_slot,
        });
    }
    let cert = root_bank.verify_certificate(cert)?;
    Ok(cert)
}
