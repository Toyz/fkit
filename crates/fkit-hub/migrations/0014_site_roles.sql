-- Site-level roles, replacing a single `is_admin` boolean.
--
-- One flag could only say "this person can do everything" or "this person is
-- ordinary", and ordinary meant they could create repositories. There was no
-- way to express the thing a public instance actually wants: let someone sign
-- up, read, file issues and comment, without also handing them the ability to
-- create repositories on your server.
--
-- Three roles, each a fixed set of capabilities:
--
--   admin     the instance: users, settings, and every repository
--   member    create and own repositories; everything observer can do
--   observer  read what is public, open issues, comment
--
-- Repository access is a separate question and is unchanged: owner,
-- collaborator role, and visibility still decide it. A site role says what you
-- may do *to the instance*, not what you may do to someone's repository.
ALTER TABLE users
    ADD COLUMN site_role TEXT NOT NULL DEFAULT 'member'
        CHECK (site_role IN ('admin', 'member', 'observer'));

-- Nobody loses anything they had. An admin stays an admin, and everyone else
-- keeps the repository-creating ability they have been using.
UPDATE users SET site_role = CASE WHEN is_admin THEN 'admin' ELSE 'member' END;

-- `is_admin` is read in fifty-odd places and every one of them stays correct,
-- because it becomes a view of the new column rather than a second copy of the
-- fact. Generated columns cannot be written, so any code still trying to *set*
-- it fails loudly instead of silently disagreeing with site_role — which is
-- exactly the failure mode a hand-synced duplicate would have had.
ALTER TABLE users DROP COLUMN is_admin;
ALTER TABLE users
    ADD COLUMN is_admin BOOLEAN
        GENERATED ALWAYS AS (site_role = 'admin') STORED;

CREATE INDEX users_site_role_idx ON users (site_role);

-- What a new account gets. Defaults to the cautious answer: an open instance
-- should be able to accept sign-ups without also accepting repositories from
-- anyone who finds it.
ALTER TABLE instance_settings
    ADD COLUMN default_site_role TEXT NOT NULL DEFAULT 'observer'
        CHECK (default_site_role IN ('admin', 'member', 'observer'));

-- An invite can name the role it grants, so "join and help triage" and "join
-- and push code" are different invitations rather than the same one followed
-- by a promotion nobody remembers to do.
ALTER TABLE invites
    ADD COLUMN site_role TEXT
        CHECK (site_role IS NULL OR site_role IN ('admin', 'member', 'observer'));

UPDATE invites SET site_role = CASE WHEN is_admin THEN 'admin' ELSE NULL END;
