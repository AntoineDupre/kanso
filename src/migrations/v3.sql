-- A human reply to a blocked agent is an ordinary comment plus the delivery
-- attempt that forwarded it into the card's OpenChamber session.
ALTER TABLE comments ADD COLUMN delivery_status TEXT;            -- sent|failed|unknown
ALTER TABLE comments ADD COLUMN delivery_error TEXT;             -- upstream error, max 500 chars
ALTER TABLE comments ADD COLUMN delivery_session_id TEXT;
ALTER TABLE comments ADD COLUMN delivery_session_directory TEXT;
ALTER TABLE comments ADD COLUMN reply_prompt TEXT;               -- exact prompt, for a byte-identical retry
