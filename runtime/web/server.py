#!/usr/bin/env python3

import asyncio
import mimetypes
import os
import uuid
from datetime import datetime
from pathlib import Path
from urllib.parse import quote

from aiohttp import ClientSession, ClientTimeout, WSMsgType, web
import llm_settings


WEB_ROOT = Path(os.environ.get("CODEX_AGENT_WEB_ROOT", "/data/web"))
WORKSPACE_ROOT = Path(os.environ.get("CODEX_AGENT_WORKSPACE", "/workspace"))
CORE_BASE_URL = os.environ.get("CODEX_AGENT_CORE_URL", "http://127.0.0.1:8090").rstrip("/")
PORT = int(os.environ.get("PORT", "8091"))
MAX_PROJECT_FILE_SIZE = 64 * 1024 * 1024
MAX_UPLOAD_SIZE = int(os.environ.get("CODEX_AGENT_UPLOAD_MAX_BYTES", str(128 * 1024 * 1024)))
HOP_BY_HOP_HEADERS = {
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
}
WEBSOCKET_HANDSHAKE_HEADERS = {
    "sec-websocket-accept",
    "sec-websocket-extensions",
    "sec-websocket-key",
    "sec-websocket-protocol",
    "sec-websocket-version",
}


def projects_enabled():
    value = os.environ.get("CODEX_AGENT_PROJECTS_ENABLED", "").strip().lower()
    return value in {"1", "true", "yes", "on"}


def safe_project_name(name):
    return (
        bool(name)
        and not name.startswith(".")
        and "/" not in name
        and "\\" not in name
        and name != "webapps"
        and Path(name).name == name
    )


def project_path(name, must_exist=True):
    if not safe_project_name(name):
        raise web.HTTPBadRequest(text="Invalid project name")
    root = WORKSPACE_ROOT.resolve()
    candidate = root / name
    try:
        resolved = candidate.resolve(strict=must_exist)
    except FileNotFoundError as error:
        raise web.HTTPNotFound(text="Project not found") from error
    if resolved.parent != root:
        raise web.HTTPBadRequest(text="Project is outside the workspace")
    if must_exist and not resolved.is_dir():
        raise web.HTTPNotFound(text="Project not found")
    return resolved


def safe_relative_path(value):
    path = Path(value)
    return bool(value) and not path.is_absolute() and ".." not in path.parts


def file_path(project, relative):
    root = project_path(project)
    if not safe_relative_path(relative):
        raise web.HTTPNotFound()
    try:
        candidate = (root / relative).resolve(strict=True)
    except FileNotFoundError as error:
        raise web.HTTPNotFound() from error
    if root not in candidate.parents or not candidate.is_file():
        raise web.HTTPNotFound()
    return candidate


def list_directory(directory, root):
    entries = []
    with os.scandir(directory) as children:
        ordered = sorted(children, key=lambda entry: entry.name)
    for entry in ordered:
        if entry.is_symlink():
            continue
        path = Path(entry.path)
        relative = path.relative_to(root).as_posix()
        mtime = datetime.fromtimestamp(entry.stat().st_mtime).strftime("%m-%d %H:%M")
        if entry.is_dir(follow_symlinks=False):
            entries.append(
                {
                    "name": entry.name,
                    "path": relative,
                    "kind": "directory",
                    "mtime": mtime,
                }
            )
        elif entry.is_file(follow_symlinks=False):
            entries.append(
                {
                    "name": entry.name,
                    "path": relative,
                    "kind": "file",
                    "size": entry.stat().st_size,
                    "mtime": mtime,
                    "children": [],
                }
            )
    return entries


async def index(_request):
    response = web.FileResponse(WEB_ROOT / "index.html")
    response.headers["Cache-Control"] = "no-store"
    return response


async def settings_page(_request):
    return web.FileResponse(WEB_ROOT / "llm-settings.html", headers={"Cache-Control": "no-store"})


async def get_llm_settings(request):
    settings = await asyncio.to_thread(llm_settings.load)
    return web.json_response({"settings": settings, "restart_required": settings != request.app["active_llm_settings"]}, headers={"Cache-Control": "no-store"})


async def save_llm_settings(request):
    if request.content_length is not None and request.content_length > 65536:
        raise web.HTTPRequestEntityTooLarge(max_size=65536, actual_size=request.content_length)
    try:
        settings = llm_settings.validate(await request.json())
    except (ValueError, TypeError) as error:
        return web.json_response({"error": str(error)}, status=400)
    settings = await asyncio.to_thread(llm_settings.save, settings)
    return web.json_response({"settings": settings, "restart_required": settings != request.app["active_llm_settings"]})


async def list_projects(_request):
    if not projects_enabled():
        return web.json_response({"enabled": False, "projects": []})
    WORKSPACE_ROOT.mkdir(parents=True, exist_ok=True)
    names = []
    with os.scandir(WORKSPACE_ROOT) as entries:
        for entry in entries:
            if safe_project_name(entry.name) and entry.is_dir(follow_symlinks=False):
                names.append(entry.name)
    return web.json_response({"enabled": True, "projects": sorted(names)})


async def create_project(request):
    if not projects_enabled():
        raise web.HTTPNotFound(text="Project management is disabled")
    try:
        body = await request.json()
    except ValueError as error:
        raise web.HTTPBadRequest(text="Invalid JSON") from error
    name = str(body.get("name", "")).strip()
    path = project_path(name, must_exist=False)
    try:
        path.mkdir()
    except FileExistsError as error:
        raise web.HTTPConflict(text="Project already exists") from error
    return web.json_response({"name": name})


async def delete_project(request):
    project_path(request.match_info["project"])
    return await proxy_http(request)


async def project_files(request):
    root = project_path(request.match_info["project"])
    relative = request.query.get("directory", "")
    if relative and not safe_relative_path(relative):
        raise web.HTTPNotFound()
    try:
        directory = (root / relative).resolve(strict=True)
    except FileNotFoundError as error:
        raise web.HTTPNotFound() from error
    if (directory != root and root not in directory.parents) or not directory.is_dir():
        raise web.HTTPNotFound()
    entries = await asyncio.to_thread(list_directory, directory, root)
    return web.json_response({"project": request.match_info["project"], "directory": relative, "entries": entries})


async def project_file(request):
    candidate = file_path(request.match_info["project"], request.match_info["path"])
    download = request.query.get("download") == "1"
    if not download and candidate.stat().st_size > MAX_PROJECT_FILE_SIZE:
        raise web.HTTPRequestEntityTooLarge(
            max_size=MAX_PROJECT_FILE_SIZE, actual_size=candidate.stat().st_size
        )
    headers = {
        "Content-Type": mimetypes.guess_type(candidate.name)[0] or "application/octet-stream",
        "X-Content-Type-Options": "nosniff",
    }
    if download:
        headers["Content-Disposition"] = (
            f"attachment; filename*=UTF-8''{quote(candidate.name)}"
        )
    return web.FileResponse(
        candidate,
        headers=headers,
    )


async def upload_project_files(request):
    root = project_path(request.match_info["project"])
    relative = request.query.get("directory", "")
    if relative:
        if not safe_relative_path(relative):
            raise web.HTTPBadRequest(text="Invalid upload directory")
        try:
            upload_root = (root / relative).resolve(strict=True)
        except FileNotFoundError as error:
            raise web.HTTPNotFound(text="Upload directory not found") from error
        if root not in upload_root.parents or not upload_root.is_dir():
            raise web.HTTPBadRequest(text="Invalid upload directory")
    else:
        upload_root = root
    reader = await request.multipart()
    saved = []
    total_size = 0
    while True:
        part = await reader.next()
        if part is None:
            break
        if part.name != "file" or not part.filename:
            continue
        filename = Path(part.filename).name
        if not filename or filename in {".", ".."}:
            raise web.HTTPBadRequest(text="Invalid file name")
        destination = upload_root / filename
        temporary = upload_root / f".upload-{uuid.uuid4().hex}"
        try:
            with temporary.open("wb") as output:
                while True:
                    chunk = await part.read_chunk()
                    if not chunk:
                        break
                    total_size += len(chunk)
                    if MAX_UPLOAD_SIZE > 0 and total_size > MAX_UPLOAD_SIZE:
                        raise web.HTTPRequestEntityTooLarge(
                            max_size=MAX_UPLOAD_SIZE, actual_size=total_size
                        )
                    output.write(chunk)
            os.replace(temporary, destination)
            saved.append(filename)
        finally:
            temporary.unlink(missing_ok=True)
    if not saved:
        raise web.HTTPBadRequest(text="No files were uploaded")
    return web.json_response(
        {
            "project": request.match_info["project"],
            "directory": relative,
            "files": saved,
        }
    )


def forwarded_headers(headers):
    return {
        name: value
        for name, value in headers.items()
        if name.lower() not in HOP_BY_HOP_HEADERS and name.lower() not in {"host", "content-length"}
    }


async def proxy_http(request):
    target = f"{CORE_BASE_URL}{request.rel_url}"
    client = request.app["client"]
    body = request.content.iter_any() if request.can_read_body else None
    upstream = await client.request(
        request.method,
        target,
        headers=forwarded_headers(request.headers),
        data=body,
        allow_redirects=False,
    )
    headers = forwarded_headers(upstream.headers)
    response = web.StreamResponse(status=upstream.status, reason=upstream.reason, headers=headers)
    await response.prepare(request)
    try:
        async for chunk in upstream.content.iter_any():
            await response.write(chunk)
        await response.write_eof()
    finally:
        upstream.release()
    return response


async def proxy_websocket(request):
    client = request.app["client"]
    headers = {
        name: value
        for name, value in forwarded_headers(request.headers).items()
        if name.lower() not in WEBSOCKET_HANDSHAKE_HEADERS
    }
    upstream = await client.ws_connect(
        f"{CORE_BASE_URL}{request.rel_url}",
        headers=headers,
        max_msg_size=16 * 1024 * 1024,
    )
    downstream = web.WebSocketResponse(max_msg_size=16 * 1024 * 1024)
    await downstream.prepare(request)

    async def downstream_to_upstream():
        async for message in downstream:
            if message.type == WSMsgType.TEXT:
                await upstream.send_str(message.data)
            elif message.type == WSMsgType.BINARY:
                await upstream.send_bytes(message.data)
            elif message.type == WSMsgType.CLOSE:
                await upstream.close()

    async def upstream_to_downstream():
        async for message in upstream:
            if message.type == WSMsgType.TEXT:
                await downstream.send_str(message.data)
            elif message.type == WSMsgType.BINARY:
                await downstream.send_bytes(message.data)
            elif message.type == WSMsgType.CLOSE:
                await downstream.close()

    tasks = {
        asyncio.create_task(downstream_to_upstream()),
        asyncio.create_task(upstream_to_downstream()),
    }
    done, pending = await asyncio.wait(tasks, return_when=asyncio.FIRST_COMPLETED)
    for task in pending:
        task.cancel()
    await asyncio.gather(*done, *pending, return_exceptions=True)
    await upstream.close()
    await downstream.close()
    return downstream


async def start_client(app):
    app["client"] = ClientSession(
        timeout=ClientTimeout(total=None, connect=10), auto_decompress=False
    )


async def stop_client(app):
    await app["client"].close()


def build_app():
    app = web.Application(client_max_size=MAX_UPLOAD_SIZE)
    app["active_llm_settings"] = llm_settings.load()
    app.on_startup.append(start_client)
    app.on_cleanup.append(stop_client)
    app.router.add_get("/", index)
    app.router.add_get("/settings", settings_page)
    app.router.add_get("/api/llm-settings", get_llm_settings)
    app.router.add_put("/api/llm-settings", save_llm_settings)
    app.router.add_post("/api/llm-settings/test", llm_settings.test_connection)
    app.router.add_get("/v1/projects", list_projects)
    app.router.add_post("/v1/projects", create_project)
    app.router.add_delete("/v1/projects/{project}", delete_project)
    app.router.add_get("/v1/projects/{project}/files", project_files)
    app.router.add_post("/v1/projects/{project}/files", upload_project_files)
    app.router.add_get("/v1/projects/{project}/files/{path:.*}", project_file)
    app.router.add_get("/v1/agent/ws", proxy_websocket)
    app.router.add_route("*", "/{path:.*}", proxy_http)
    return app


if __name__ == "__main__":
    web.run_app(build_app(), host="0.0.0.0", port=PORT, access_log=None)
