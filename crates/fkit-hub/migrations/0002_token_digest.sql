-- Session and access tokens are now stored as a fast digest, not Argon2.
--
-- Argon2 exists to make *low-entropy* secrets (human passwords) expensive to
-- guess. Session tokens and PATs are 256 bits from the OS CSPRNG: no amount of
-- hashing speed helps an attacker brute-force them, so paying ~15 ms per
-- request to verify one is pure cost. Passwords still use Argon2id.
--
-- A digest is also directly indexable, so authentication becomes a single
-- indexed equality lookup rather than "find candidate by prefix, then verify".

DELETE FROM sessions;
DELETE FROM access_tokens;

CREATE UNIQUE INDEX access_tokens_hash_idx ON access_tokens (token_hash);
