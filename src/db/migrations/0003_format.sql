ALTER TABLE request_logs ADD COLUMN format TEXT;
CREATE INDEX IF NOT EXISTS idx_req_format ON request_logs(format, seq)
