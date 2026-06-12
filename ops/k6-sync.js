import http from "k6/http";
import { check } from "k6";

export const options = {
  scenarios: {
    sync: {
      executor: "ramping-vus",
      startVUs: 1,
      stages: [
        { duration: "30s", target: Number(__ENV.VUS || 20) },
        { duration: __ENV.DURATION || "5m", target: Number(__ENV.VUS || 20) },
        { duration: "30s", target: 0 },
      ],
    },
  },
  thresholds: {
    http_req_failed: ["rate<0.01"],
    http_req_duration: ["p(95)<1000"],
  },
};

const payload = JSON.parse(open(__ENV.PAYLOAD_FILE || "./sync-payload.json"));

export default function () {
  const response = http.post(`${__ENV.BASE_URL}/sync`, JSON.stringify(payload), {
    headers: {
      Authorization: `Bearer ${__ENV.ACCESS_TOKEN}`,
      "Content-Type": "application/json",
      "x-device-id": `${__ENV.DEVICE_PREFIX || "k6"}-${__VU}`,
    },
  });
  check(response, { "sync accepted": (result) => result.status === 200 });
}
