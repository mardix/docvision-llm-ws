CREATE TABLE IF NOT EXISTS request_logs (
  seq BIGSERIAL PRIMARY KEY,
  request_id TEXT NOT NULL UNIQUE,
  operation TEXT NOT NULL,
  mode TEXT,
  source TEXT,
  options TEXT,
  metadata TEXT,
  request_status TEXT NOT NULL,
  execution_status TEXT,
  execution_stage TEXT,
  job_id TEXT,
  http_status BIGINT,
  created_at BIGINT NOT NULL,
  finished_at BIGINT,
  timings TEXT,
  statistics TEXT,
  usage TEXT,
  warnings TEXT,
  error TEXT,
  result_available BIGINT NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_req_created ON request_logs(created_at);
CREATE INDEX IF NOT EXISTS idx_req_op ON request_logs(operation, seq);
CREATE INDEX IF NOT EXISTS idx_req_mode_status ON request_logs(mode, execution_status, seq);
CREATE INDEX IF NOT EXISTS idx_req_job ON request_logs(job_id);
CREATE TABLE IF NOT EXISTS request_events (
  id BIGSERIAL PRIMARY KEY,
  request_id TEXT NOT NULL,
  at BIGINT NOT NULL,
  stage TEXT NOT NULL,
  event TEXT NOT NULL,
  detail TEXT
);
CREATE INDEX IF NOT EXISTS idx_events_req ON request_events(request_id, id);
CREATE TABLE IF NOT EXISTS jobs (
  job_id TEXT PRIMARY KEY,
  request_id TEXT NOT NULL,
  state TEXT NOT NULL,
  priority TEXT NOT NULL,
  created_at BIGINT NOT NULL,
  started_at BIGINT,
  finished_at BIGINT,
  result_path TEXT,
  error TEXT,
  secrets BYTEA,
  cache_key TEXT,
  has_webhook BIGINT NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_jobs_state ON jobs(state);
CREATE INDEX IF NOT EXISTS idx_jobs_created ON jobs(created_at);
CREATE INDEX IF NOT EXISTS idx_jobs_request ON jobs(request_id);
CREATE TABLE IF NOT EXISTS llm_calls (
  id BIGSERIAL PRIMARY KEY,
  call_id TEXT NOT NULL,
  request_id TEXT NOT NULL,
  record TEXT NOT NULL,
  created_at BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_calls_req ON llm_calls(request_id, id);
CREATE TABLE IF NOT EXISTS webhook_deliveries (
  event_id TEXT PRIMARY KEY,
  job_id TEXT NOT NULL,
  status TEXT NOT NULL,
  attempts BIGINT NOT NULL DEFAULT 0,
  last_http_status BIGINT,
  last_error TEXT,
  next_attempt_at BIGINT,
  created_at BIGINT NOT NULL,
  delivered_at BIGINT
);
CREATE INDEX IF NOT EXISTS idx_wh_job ON webhook_deliveries(job_id);
CREATE INDEX IF NOT EXISTS idx_wh_status ON webhook_deliveries(status);
CREATE TABLE IF NOT EXISTS result_cache (
  cache_key TEXT PRIMARY KEY,
  result_path TEXT NOT NULL,
  body_start BIGINT NOT NULL,
  body_end BIGINT NOT NULL,
  size BIGINT NOT NULL,
  created_at BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_cache_created ON result_cache(created_at);
