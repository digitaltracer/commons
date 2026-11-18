-- The email an invite is for, normalised. NULL only on invites made before this, which are
-- no longer valid.
ALTER TABLE invites ADD COLUMN email text;
