"""Onboards this Home Assistant, adds its MCP Server integration, exposes the demo lock to Assist,
and prints a long-lived access token. Runs inside the container, on the Python it ships."""

import asyncio
import sys

import aiohttp

BASE = "http://127.0.0.1:8123"
CLIENT_ID = BASE + "/"
STARTUP_POLLS = 300
POLL_SECONDS = 2
RUNNING = "RUNNING"
TOKEN_DAYS = 1
EXPOSED = ["lock.front_door"]
USER = {
    "client_id": CLIENT_ID,
    "name": "Caudra",
    "username": "caudra",
    "password": "caudra-mcp-it",
    "language": "en",
}
ONBOARDING_STEPS = (
    ("core_config", {}),
    ("analytics", {}),
    ("integration", {"client_id": CLIENT_ID, "redirect_uri": CLIENT_ID + "?auth_callback=1"}),
)


def fail(message):
    sys.exit(f"Home Assistant setup failed: {message}")


def bearer(token):
    return {"Authorization": f"Bearer {token}"}


async def request(http, method, path, token=None, form=None, body=None):
    headers = bearer(token) if token else {}
    async with http.request(method, BASE + path, headers=headers, data=form, json=body) as response:
        if response.status >= 400:
            fail(f"{method} {path}: {response.status} {await response.text()}")
        return await response.json(content_type=None)


async def poll(http, path, token, ready):
    for _ in range(STARTUP_POLLS):
        try:
            async with http.get(BASE + path, headers=bearer(token) if token else {}) as response:
                if response.status == 200 and ready(await response.json(content_type=None)):
                    return
        except aiohttp.ClientError:
            pass
        await asyncio.sleep(POLL_SECONDS)
    fail(f"{path} never became ready")


async def add_mcp_server(http, token):
    flow = await request(http, "POST", "/api/config/config_entries/flow", token, body={"handler": "mcp_server"})
    if flow.get("type") == "form":
        answer = {}
        for field in flow.get("data_schema", []):
            if field["name"] == "llm_hass_api":
                multiple = field.get("selector", {}).get("select", {}).get("multiple", False)
                answer["llm_hass_api"] = ["assist"] if multiple else "assist"
        flow = await request(http, "POST", f"/api/config/config_entries/flow/{flow['flow_id']}", token, body=answer)
    if flow.get("type") != "create_entry":
        fail(f"the MCP Server integration was not added: {flow}")


async def websocket(http, token, *messages):
    async with http.ws_connect(BASE + "/api/websocket") as socket:
        await socket.receive_json()
        await socket.send_json({"type": "auth", "access_token": token})
        if (await socket.receive_json())["type"] != "auth_ok":
            fail("the websocket refused the token")
        results = []
        for index, message in enumerate(messages, 1):
            await socket.send_json({"id": index, **message})
            reply = await socket.receive_json()
            while reply.get("id") != index or reply.get("type") != "result":
                reply = await socket.receive_json()
            if not reply["success"]:
                fail(f"{message['type']}: {reply}")
            results.append(reply["result"])
        return results


async def main():
    async with aiohttp.ClientSession() as http:
        await poll(http, "/api/onboarding", None, lambda steps: True)
        user = await request(http, "POST", "/api/onboarding/users", body=USER)
        tokens = await request(
            http,
            "POST",
            "/auth/token",
            form={"grant_type": "authorization_code", "code": user["auth_code"], "client_id": CLIENT_ID},
        )
        access = tokens["access_token"]
        for step, body in ONBOARDING_STEPS:
            await request(http, "POST", f"/api/onboarding/{step}", access, body=body)
        await poll(http, "/api/config", access, lambda config: config.get("state") == RUNNING)
        await add_mcp_server(http, access)
        long_lived, _ = await websocket(
            http,
            access,
            {"type": "auth/long_lived_access_token", "client_name": "caudra-mcp-it", "lifespan": TOKEN_DAYS},
            {
                "type": "homeassistant/expose_entity",
                "assistants": ["conversation"],
                "entity_ids": EXPOSED,
                "should_expose": True,
            },
        )
        print(long_lived)


asyncio.run(main())
