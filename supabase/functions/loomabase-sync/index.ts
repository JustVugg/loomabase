Deno.serve(async (request) => {
  if (request.method !== "POST") {
    return new Response("method not allowed", { status: 405 });
  }

  const target = Deno.env.get("LOOMABASE_SYNC_URL");
  const authorization = request.headers.get("authorization");
  const deviceId = request.headers.get("x-device-id");
  if (!target) {
    return new Response("proxy is not configured", { status: 503 });
  }
  if (!authorization || !deviceId) {
    return new Response("missing authentication or device identity", {
      status: 401,
    });
  }

  return fetch(target, {
    method: "POST",
    headers: {
      "authorization": authorization,
      "content-type": "application/json",
      "x-device-id": deviceId,
    },
    body: request.body,
    signal: AbortSignal.timeout(30_000),
  });
});
