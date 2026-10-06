ALTER TABLE request_logs ADD COLUMN requester_id TEXT;
CREATE INDEX IF NOT EXISTS idx_req_requester ON request_logs(requester_id, seq)
