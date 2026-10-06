// Burst test: async submissions far beyond queue depth must get 429 + Retry-After quickly,
// while admitted work keeps its latency. Run: k6 run -e TOKEN=... -e SOURCE=/abs/doc.pdf scripts/load/k6_burst.js
import http from 'k6/http';
import { check } from 'k6';

export const options = {
  scenarios: { burst: { executor: 'constant-arrival-rate', rate: 2000, timeUnit: '1s', duration: '20s', preAllocatedVUs: 500, maxVUs: 2000 } },
  thresholds: { 'http_req_duration{status:429}': ['p(99)<50'], 'http_req_duration{status:202}': ['p(99)<100'] },
};

const URL = __ENV.URL || 'http://127.0.0.1:4242/rpc';
export default function () {
  const body = JSON.stringify({ operation: 'convert', payload: { source: __ENV.SOURCE }, options: { cache: 'bypass', gen_summary: false, gen_title: false } });
  const r = http.post(URL, body, { headers: { 'Content-Type': 'application/json', 'X-ACCESS-TOKEN': __ENV.TOKEN } });
  check(r, {
    'accepted or shed': (x) => x.status === 202 || x.status === 429,
    '429 has Retry-After': (x) => x.status !== 429 || x.headers['Retry-After'] !== undefined,
  });
}
