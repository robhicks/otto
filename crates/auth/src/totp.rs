//! RFC 6238 time-based one-time passwords, plus the replay floor and lockout bookkeeping.
//!
//! The crypto is two small functions — HMAC-SHA1 over the 8-byte big-endian step and the RFC 4226
//! 31-bit dynamic truncation — verified against the RFC 6238 Appendix B test vectors. The
//! [`TotpVerifier`] layers the security decisions on top of the [`AuthStore`]: a ±1 step skew
//! window, a replay floor (`step <= last_step` can never be accepted), a conditional
//! `set_last_step` write so concurrent same-step logins have exactly one winner, and a per-user
//! lockout (5 failures within 15 minutes) that short-circuits before computing a code. A replay —
//! a code that belongs to an already-consumed step, or presented when every candidate is behind
//! the floor — is rejected without counting toward the lockout, so a client's own retry of a lost
//! response can never lock it out.
//!
//! Everything is a pure function of `(secret, last_step, now)`, so a [`Clock`] trait — the
//! [`SystemClock`] in production, a [`FixedClock`] in tests — keeps the RFC vectors and the whole
//! suite deterministic.

use std::sync::Arc;
use std::time::Duration;

use hmac::{Hmac, Mac};
use otto_protocol::UserId;
use sha1::Sha1;
use subtle::ConstantTimeEq;

use crate::store::AuthStore;

type HmacSha1 = Hmac<Sha1>;

/// The RFC 6238 default step: one code per 30 seconds.
pub const STEP_SECS: u64 = 30;
/// The skew window: accept `T-1`, `T`, `T+1`.
pub const SKEW: i64 = 1;
/// The failure budget before the user is locked out (spec §6.1, A7).
pub const MAX_FAILURES: u32 = 5;
/// The lockout window: failures inside the trailing 15 minutes accumulate (spec A7).
pub const FAILURE_WINDOW: Duration = Duration::from_secs(15 * 60);

/// A source of unix seconds. Injected so every time-dependent behavior is testable with a fixed
/// clock (the RFC 6238 vectors are defined at specific timestamps).
pub trait Clock: Send + Sync {
    /// Seconds since the Unix epoch.
    fn now(&self) -> u64;
}

/// The system wall clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default()
    }
}

/// A fixed clock for tests and other deterministic uses.
#[derive(Debug, Clone, Copy)]
pub struct FixedClock(pub u64);

impl Clock for FixedClock {
    fn now(&self) -> u64 {
        self.0
    }
}

/// RFC 4226 §5.3 dynamic truncation: the 31-bit value derived from the HMAC-SHA1 of the 8-byte
/// big-endian step. Formatted `{:08}` this reproduces the RFC 6238 Appendix B vectors.
pub fn truncate(secret: &[u8], step: u64) -> u32 {
    let mut mac = HmacSha1::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(&step.to_be_bytes());
    let digest = mac.finalize().into_bytes();
    let offset = (digest[digest.len() - 1] & 0x0f) as usize;
    u32::from_be_bytes([
        digest[offset],
        digest[offset + 1],
        digest[offset + 2],
        digest[offset + 3],
    ]) & 0x7fff_ffff
}

/// The 6-digit, zero-padded TOTP code for `step` — what authenticator apps show.
pub fn totp_at(secret: &[u8], step: u64) -> String {
    format!("{:06}", truncate(secret, step) % 1_000_000)
}

/// The TOTP verification core: skew, replay, concurrency, and lockout over the [`AuthStore`].
///
/// A verification is a pure function of `(secret, last_step, now)`: give the verifier a
/// [`FixedClock`], or call [`TotpVerifier::verify_at`] with an explicit `now`, and the outcome is
/// deterministic.
#[derive(Clone)]
pub struct TotpVerifier {
    store: Arc<dyn AuthStore>,
    clock: Arc<dyn Clock>,
}

impl TotpVerifier {
    pub fn new(store: Arc<dyn AuthStore>, clock: Arc<dyn Clock>) -> Self {
        Self { store, clock }
    }

    /// Verify `code` for `user` at the clock's current time.
    pub async fn verify(&self, user: &UserId, code: &str) -> Result<(), TotpError> {
        let now = self.clock.now();
        self.verify_at(user, code, now).await
    }

    /// Verify `code` for `user` at an explicit `now` (unix seconds).
    pub async fn verify_at(&self, user: &UserId, code: &str, now: u64) -> Result<(), TotpError> {
        let now = now as i64;
        // Lockout first: a locked user is rejected without even computing a code — the 10^6
        // keyspace makes this a correctness requirement, not a nicety (spec §6.1).
        let failures = self
            .store
            .failures_within(user, now, FAILURE_WINDOW)
            .await?;
        if failures >= MAX_FAILURES {
            // The lockout expires when the trailing window elapses; returning the whole window
            // errs on the side of not revealing how close the cooldown is.
            return Err(TotpError::RateLimited {
                retry_after_secs: FAILURE_WINDOW.as_secs(),
            });
        }

        let Some(secret_b32) = self.store.totp_secret(user).await? else {
            // An unknown user is indistinguishable from a wrong code (spec A11), and there is no
            // row to count a failure against.
            return Err(TotpError::Invalid);
        };
        let secret = data_encoding::BASE32_NOPAD
            .decode(secret_b32.as_bytes())
            .map_err(|e| anyhow::anyhow!("stored TOTP secret is not valid base32: {e}"))?;

        let last_step = self.store.last_step(user).await?;
        let t = now.div_euclid(STEP_SECS as i64) as u64;
        let mut accepted = None;
        // `saw_in_window` records that at least one candidate escaped the replay floor, and
        // `replay` records that the presented code belongs to a floor-rejected (already-consumed)
        // step. Together they decide whether a rejection is a genuine failure to count toward the
        // lockout, or a replay that must not be counted.
        let mut saw_in_window = false;
        let mut replay = false;
        for candidate in (t.saturating_sub(SKEW as u64))..=(t.saturating_add(SKEW as u64)) {
            // The replay floor: any candidate step at or below the accepted one is rejected
            // BEFORE it can be accepted — without it the ±1 window would make an observed code
            // replayable for 90 seconds (spec §6.1). The code is still compared against a
            // floor-rejected candidate, but only to recognize a replay for the failure counter:
            // it can never be accepted from one.
            if candidate <= last_step {
                replay |= codes_equal(&secret, code, candidate);
                continue;
            }
            saw_in_window = true;
            if codes_equal(&secret, code, candidate) {
                accepted = Some(candidate);
                break;
            }
        }

        match accepted {
            Some(step) => {
                // The conditional write is the concurrent-winner decision: two logins racing on
                // the same step both get here, and sqlite serializes the UPDATE so exactly one
                // observes `WHERE last_step < ?`. A win also clears the failure counter.
                if self.store.set_last_step(user, step).await? {
                    Ok(())
                } else {
                    // A valid code that lost the same-step race is a replay, not a failed login —
                    // counting it would let a user's own second device lock them out.
                    Err(TotpError::Replay)
                }
            }
            None => {
                // A rejection is a failed login only when the code was genuinely wrong at an
                // in-window step. A replay — the presented code belongs to an already-consumed
                // step, or every candidate is behind the floor — is not: counting it would let a
                // client's own retry of a lost response lock it out.
                if saw_in_window && !replay {
                    self.store.record_failure(user, now, FAILURE_WINDOW).await?;
                }
                Err(TotpError::Invalid)
            }
        }
    }
}

/// Constant-time digit comparison — never `==` (spec §6.1).
fn codes_equal(secret: &[u8], provided: &str, step: u64) -> bool {
    let expected = totp_at(secret, step);
    provided.as_bytes().ct_eq(expected.as_bytes()).into()
}

/// Why a TOTP login was rejected.
#[derive(Debug)]
pub enum TotpError {
    /// No code matched at any allowed step — a wrong code, or an unknown user (spec A11 keeps
    /// the two indistinguishable).
    Invalid,
    /// The code's step is at or below the replay floor, or the login lost the concurrent
    /// same-step race. Not counted as a failure: the code was already accepted once.
    Replay,
    /// Too many recent failures; the attempt was rejected without computing a code.
    RateLimited { retry_after_secs: u64 },
    /// The backend itself failed.
    Backend(anyhow::Error),
}

impl From<anyhow::Error> for TotpError {
    fn from(e: anyhow::Error) -> Self {
        TotpError::Backend(e)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use otto_protocol::UserId;

    use super::*;
    use crate::store::SqliteAuthStore;

    /// The RFC 6238 Appendix B test secret: the ASCII bytes of "12345678901234567890".
    fn rfc_secret() -> &'static [u8] {
        b"12345678901234567890"
    }

    /// The RFC 6238 Appendix B SHA-1 vectors: (time in seconds, 8-digit TOTP). The published
    /// values are 8 digits (RFC 4226) — the last six of which are the 6-digit form — so they
    /// assert the raw truncation, not the 6-digit formatter. The RFC's first column is the
    /// wall-clock time, not the step: the step is `time / 30` (the RFC's `T (hex)` column).
    ///
    /// Note: the row at time 1111111111 is `14050471` per RFC 6238 Table 1 — the frequently
    /// mis-transcribed `14050431` is not what the RFC publishes, and this implementation
    /// reproduces the RFC's value.
    const RFC_VECTORS: [(u64, &str); 6] = [
        (59, "94287082"),
        (1111111109, "07081804"),
        (1111111111, "14050471"),
        (1234567890, "89005924"),
        (2000000000, "69279037"),
        (20000000000, "65353130"),
    ];

    fn alice() -> UserId {
        UserId::parse("alice").unwrap()
    }

    async fn temp_store() -> (SqliteAuthStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteAuthStore::open(dir.path().join("auth.db"))
            .await
            .unwrap();
        (store, dir)
    }

    /// A verifier over a fresh store with a fixed clock.
    async fn verifier_with_fixed_clock(
        now: u64,
    ) -> (TotpVerifier, SqliteAuthStore, tempfile::TempDir) {
        let (store, dir) = temp_store().await;
        let verifier = TotpVerifier::new(Arc::new(store.clone()), Arc::new(FixedClock(now)));
        (verifier, store, dir)
    }

    #[test]
    fn raw_truncation_matches_the_rfc_6238_vectors() {
        for (time, expected) in RFC_VECTORS {
            let step = time / STEP_SECS;
            // RFC 4226 §5.4: TOTP = DT mod 10^Digits, so the published 8-digit value is the
            // 31-bit dynamic truncation reduced mod 10^8.
            assert_eq!(
                format!("{:08}", truncate(rfc_secret(), step) % 100_000_000),
                expected,
                "time {time} (step {step})"
            );
        }
    }

    #[test]
    fn production_formatter_is_six_digits_zero_padded() {
        for (time, expected_8) in RFC_VECTORS {
            let step = time / STEP_SECS;
            // The 6-digit form is the last six digits of the RFC's 8-digit vector.
            let expected_6 = &expected_8[2..];
            assert_eq!(totp_at(rfc_secret(), step), expected_6, "time {time}");
        }
        // Leading zeros survive: zero-padded, never trimmed.
        assert_eq!(totp_at(rfc_secret(), 1111111109 / STEP_SECS), "081804");
    }

    #[tokio::test]
    async fn the_current_step_and_the_adjacent_steps_are_accepted() {
        let (verifier, store, _dir) = verifier_with_fixed_clock(59 * 30).await;
        store.enroll_user(&alice(), rfc_secret()).await.unwrap();

        // now = 1770 → T = 59. Each candidate step is verified against a fresh user so a
        // success on one cannot trip the next's replay floor.
        let now = 59 * 30;
        for (i, (step, label)) in [(58, "T-1"), (59, "T"), (60, "T+1")]
            .into_iter()
            .enumerate()
        {
            let user = UserId::parse(&format!("skew-{i}")).unwrap();
            store.enroll_user(&user, rfc_secret()).await.unwrap();
            let result = verifier
                .verify_at(&user, &totp_at(rfc_secret(), step), now)
                .await;
            assert!(
                result.is_ok(),
                "step {step} ({label}) should be accepted: {result:?}"
            );
        }
    }

    #[tokio::test]
    async fn steps_two_out_of_window_are_rejected() {
        let (verifier, store, _dir) = verifier_with_fixed_clock(59 * 30).await;
        store.enroll_user(&alice(), rfc_secret()).await.unwrap();

        let now = 59 * 30;
        for (i, (step, _label)) in [(57, "T-2"), (61, "T+2")].into_iter().enumerate() {
            let user = UserId::parse(&format!("skew-{i}")).unwrap();
            store.enroll_user(&user, rfc_secret()).await.unwrap();
            let result = verifier
                .verify_at(&user, &totp_at(rfc_secret(), step), now)
                .await;
            assert!(
                matches!(result, Err(TotpError::Invalid)),
                "step {step} should be rejected: {result:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_used_step_is_never_accepted_again() {
        let (verifier, store, _dir) = verifier_with_fixed_clock(59 * 30).await;
        store.enroll_user(&alice(), rfc_secret()).await.unwrap();

        let now = 59 * 30;
        assert!(
            verifier
                .verify_at(&alice(), &totp_at(rfc_secret(), 59), now)
                .await
                .is_ok()
        );
        assert_eq!(store.last_step(&alice()).await.unwrap(), 59);

        // Replay: the same code presented again at the same step is rejected. Without the
        // replay floor it would be accepted — step 59 is still inside the skew window — so this
        // rejection is the floor doing its job, and it is deliberately indistinguishable from a
        // wrong code (the floor is consulted before the code is ever compared, spec §6.1).
        assert!(
            verifier
                .verify_at(&alice(), &totp_at(rfc_secret(), 59), now)
                .await
                .is_err()
        );
        assert_eq!(store.last_step(&alice()).await.unwrap(), 59);

        // Once the window has moved on, the old code is dead by distance alone.
        assert!(
            verifier
                .verify_at(&alice(), &totp_at(rfc_secret(), 59), 61 * 30)
                .await
                .is_err()
        );
        // A genuinely new step is still accepted.
        assert!(
            verifier
                .verify_at(&alice(), &totp_at(rfc_secret(), 61), 61 * 30)
                .await
                .is_ok()
        );
        assert_eq!(store.last_step(&alice()).await.unwrap(), 61);
    }

    /// Two concurrent logins presenting the same code both pass the read and the code match; the
    /// conditional `UPDATE ... WHERE last_step < ?` lets exactly one win.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_same_step_logins_have_exactly_one_winner() {
        let (store, _dir) = temp_store().await;
        store.enroll_user(&alice(), rfc_secret()).await.unwrap();
        let verifier = TotpVerifier::new(Arc::new(store.clone()), Arc::new(FixedClock(100 * 30)));

        let code = totp_at(rfc_secret(), 100);
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let verifier = verifier.clone();
                let code = code.clone();
                tokio::spawn(async move { verifier.verify_at(&alice(), &code, 100 * 30).await })
            })
            .collect();
        let mut winners = 0;
        for handle in handles {
            if handle.await.unwrap().is_ok() {
                winners += 1;
            }
        }
        assert_eq!(
            winners, 1,
            "exactly one concurrent same-step login must win"
        );
    }

    #[tokio::test]
    async fn five_failures_lock_the_user_out_before_a_code_is_computed() {
        let (verifier, store, _dir) = verifier_with_fixed_clock(60 * 30).await;
        store.enroll_user(&alice(), rfc_secret()).await.unwrap();

        let now = 60 * 30;
        let correct = totp_at(rfc_secret(), 60);
        let wrong = if correct == "000000" {
            "000001"
        } else {
            "000000"
        };

        for _ in 0..MAX_FAILURES {
            assert!(matches!(
                verifier.verify_at(&alice(), wrong, now).await,
                Err(TotpError::Invalid)
            ));
        }
        // Locked: even the correct code is rejected without being computed against.
        assert!(matches!(
            verifier.verify_at(&alice(), &correct, now).await,
            Err(TotpError::RateLimited { .. })
        ));
    }

    #[tokio::test]
    async fn unknown_user_is_indistinguishable_from_a_wrong_code() {
        let (verifier, _store, _dir) = verifier_with_fixed_clock(59 * 30).await;
        assert!(matches!(
            verifier.verify_at(&alice(), "000000", 59 * 30).await,
            Err(TotpError::Invalid)
        ));
    }

    #[tokio::test]
    async fn replaying_a_consumed_step_does_not_count_toward_lockout() {
        let (verifier, store, _dir) = verifier_with_fixed_clock(59 * 30).await;
        store.enroll_user(&alice(), rfc_secret()).await.unwrap();

        let now = 59 * 30;
        let code = totp_at(rfc_secret(), 59);
        // Consume step 59.
        assert!(verifier.verify_at(&alice(), &code, now).await.is_ok());
        assert_eq!(store.last_step(&alice()).await.unwrap(), 59);

        // A client retrying the same code — a lost response, not a failed login — is rejected,
        // but the rejection is a replay, not a failure: it must never accumulate toward the
        // lockout threshold no matter how many times the already-consumed code is replayed.
        for _ in 0..(MAX_FAILURES * 2) {
            assert!(matches!(
                verifier.verify_at(&alice(), &code, now).await,
                Err(TotpError::Invalid)
            ));
        }
        assert_eq!(
            store
                .failures_within(&alice(), now as i64, FAILURE_WINDOW)
                .await
                .unwrap(),
            0
        );
        // A fresh step is still accepted — no lockout, and the failure counter is clean.
        assert!(matches!(
            verifier
                .verify_at(&alice(), &totp_at(rfc_secret(), 60), 60 * 30)
                .await,
            Ok(())
        ));
    }
}
