#!/usr/bin/env python3
"""
Capture REST/GraphQL artefacts from a running Kosh service.

Usage:
  python scripts/capture_service_artifacts.py \
      --output-dir parity/python \
      --base-url http://127.0.0.1:6100 \
      --elastic-url http://127.0.0.1:9200 \
      [--skip-openapi] \
      [--skip-graphql-introspection] \
      --indices kosh[demo] kosh[demo_alt] \
      -- kosh --data-root fixtures/demo --data-host http://127.0.0.1:9200 --data-sync 0 --api-port 6100
"""

from __future__ import annotations

import argparse
import json
import os
import signal
import subprocess
import sys
import time
from pathlib import Path
from typing import Dict, List, Optional, Tuple

from urllib.parse import quote

import requests


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Capture Kosh HTTP artefacts for parity checks")
    parser.add_argument(
        "--output-dir",
        required=True,
        help="Directory where artefacts will be written (will be created if missing)",
    )
    parser.add_argument(
        "--base-url",
        required=True,
        help="Base URL of the service (e.g. http://127.0.0.1:6100)",
    )
    parser.add_argument(
        "--elastic-url",
        default="http://127.0.0.1:9200",
        help="Elasticsearch endpoint used for index resets",
    )
    parser.add_argument(
        "--indices",
        nargs="+",
        default=["kosh[demo]", "kosh[demo_alt]"],
        help="Elasticsearch indices to delete before capture",
    )
    parser.add_argument(
        "--wait-seconds",
        type=int,
        default=60,
        help="Maximum seconds to wait for the service to become ready",
    )
    parser.add_argument(
        "--query-token",
        default="a",
        help="Token used for sample search queries (default: 'a')",
    )
    parser.add_argument(
        "--graphql-size",
        type=int,
        default=5,
        help="Number of results requested in GraphQL sample queries",
    )
    parser.add_argument(
        "--skip-openapi",
        action="store_true",
        help="Reuse existing OpenAPI artefacts instead of fetching them over HTTP",
    )
    parser.add_argument(
        "--skip-graphql-introspection",
        action="store_true",
        help="Reuse existing GraphQL introspection artefacts instead of querying the endpoint",
    )
    args, command = parser.parse_known_args()
    if command and command[0] == "--":
        command = command[1:]
    if not command:
        parser.error("expected service command after '--'")
    args.command = command
    return args


def reset_indices(elastic_url: str, indices: List[str]) -> None:
    for name in indices:
        index = quote(name, safe="")
        try:
            resp = requests.delete(f"{elastic_url}/{index}", timeout=5)
            if resp.status_code not in (200, 202, 404):
                print(f"Warning: failed to delete index {name}: {resp.status_code}", file=sys.stderr)
        except requests.RequestException as exc:
            print(f"Warning: error deleting index {name}: {exc}", file=sys.stderr)


def start_service(command: List[str]) -> subprocess.Popen:
    env = os.environ.copy()
    env.setdefault("PYTHONUNBUFFERED", "1")
    return subprocess.Popen(
        command,
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        preexec_fn=os.setsid,
        text=True,
    )


def stop_service(proc: subprocess.Popen) -> None:
    try:
        os.killpg(os.getpgid(proc.pid), signal.SIGTERM)
    except ProcessLookupError:
        return
    try:
        proc.wait(timeout=10)
    except subprocess.TimeoutExpired:
        os.killpg(os.getpgid(proc.pid), signal.SIGKILL)
        proc.wait(timeout=5)


def wait_for_service(base_url: str, timeout_seconds: int) -> None:
    deadline = time.time() + timeout_seconds
    last_error = None
    while time.time() < deadline:
        try:
            resp = requests.get(f"{base_url}/api", timeout=3)
            if resp.status_code == 200:
                return
            last_error = f"status {resp.status_code}"
        except requests.RequestException as exc:
            last_error = str(exc)
        time.sleep(1)
    raise RuntimeError(f"service did not become ready: {last_error}")


def normalise_spec(spec: Dict) -> Dict:
    info = spec.get("info")
    if isinstance(info, dict):
        info["version"] = "normalized"
    return spec


def normalise_entries(entries: List[Dict]) -> List[Dict]:
    normalised = []
    for entry in entries:
        item = dict(entry)
        item.pop("created", None)
        normalised.append(item)
    normalised.sort(key=lambda item: item.get("id", ""))
    return normalised


def save_json(path: Path, payload: Dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", encoding="utf-8") as handle:
        json.dump(payload, handle, indent=2, sort_keys=True)
        handle.write("\n")


def capture_artifacts(
    base_url: str,
    output_dir: Path,
    query_token: str,
    graphql_size: int,
    skip_openapi: bool,
    skip_graphql_introspection: bool,
) -> None:
    output_dir.mkdir(parents=True, exist_ok=True)

    api_resp = requests.get(f"{base_url}/api", timeout=5).json()
    save_json(output_dir / "api.json", api_resp)

    dicts = api_resp.get("dicts", {})
    for uid, metadata in sorted(dicts.items()):
        lexicon_dir = output_dir / "rest" / uid
        lexicon_dir.mkdir(parents=True, exist_ok=True)

        lex_resp = requests.get(f"{base_url}/api/{uid}", timeout=5).json()
        save_json(lexicon_dir / "metadata.json", lex_resp)

        spec_path = output_dir / "openapi" / f"{uid}.json"
        if skip_openapi:
            if not spec_path.exists():
                raise RuntimeError(f"expected OpenAPI artefact at {spec_path}")
            with spec_path.open("r", encoding="utf-8") as handle:
                spec_resp = json.load(handle)
        else:
            spec_resp = requests.get(f"{base_url}/api/{uid}/restful/spec", timeout=5).json()
        spec_resp = normalise_spec(spec_resp)
        save_json(spec_path, spec_resp)

        fields = list(
            spec_resp.get("definitions", {})
            .get("Entry", {})
            .get("properties", {})
            .keys()
        )
        field = next((f for f in fields if f not in ("xml", "created")), fields[0] if fields else "lemma")
        query_types = metadata.get("query_types", ["match"])
        query_type = query_types[0]

        def capture_entries(name: str, size: int | None) -> None:
            params = {
                "field": field,
                "query": query_token,
                "query_type": query_type,
            }
            if size is not None:
                params["size"] = size
            resp = requests.get(
                f"{base_url}/api/{uid}/restful/entries",
                params=params,
                timeout=5,
            ).json()
            entries = resp.get("data", {}).get("entries", [])
            resp["data"]["entries"] = normalise_entries(entries)
            save_json(lexicon_dir / f"entries_{name}.json", resp)

        capture_entries("default", None)
        capture_entries("size1", 1)
        capture_entries("max", 10_000)

        def fetch_ids(params: List[Tuple[str, str]]) -> Tuple[requests.Response, Optional[Dict[str, Any]]]:
            resp = requests.get(
                f"{base_url}/api/{uid}/restful/ids",
                params=params,
                timeout=5,
            )
            try:
                payload = resp.json()
            except (json.JSONDecodeError, ValueError, requests.exceptions.JSONDecodeError):
                payload = None
            return resp, payload

        ids_params = [("ids", f"{uid}-1"), ("ids", f"{uid}-2")]
        ids_resp, ids_payload = fetch_ids(ids_params)
        if ids_payload is None or ids_resp.status_code >= 400:
            ids_resp, ids_payload = fetch_ids(
                [("ids[]", f"{uid}-1"), ("ids[]", f"{uid}-2")]
            )

        if ids_payload is None:
            ids_payload = {
                "error": {
                    "status": ids_resp.status_code,
                    "body": ids_resp.text.strip(),
                }
            }
        data_section = ids_payload.get("data")
        if isinstance(data_section, dict):
            ids_entries = data_section.get("ids", [])
            data_section["ids"] = normalise_entries(ids_entries)
        save_json(lexicon_dir / "ids.json", ids_payload)

        if metadata.get("endpoints", {}).get("graphql"):
            introspection_path = output_dir / "graphql" / f"{uid}_introspection.json"
            if skip_graphql_introspection:
                if not introspection_path.exists():
                    raise RuntimeError(f"expected GraphQL introspection artefact at {introspection_path}")
            else:
                introspection = requests.post(
                    f"{base_url}/api/{uid}/graphql",
                    json={
                        "query": """
                        query IntrospectionQuery {
                          __schema {
                            queryType { name }
                            types {
                              kind
                              name
                              fields(includeDeprecated: true) {
                                name
                                type { kind name ofType { kind name } }
                              }
                              enumValues(includeDeprecated: true) { name }
                            }
                          }
                        }
                        """
                    },
                    timeout=10,
                ).json()
                save_json(introspection_path, introspection)

            field_enum = field
            query_enum = query_type
            entries_query = requests.post(
                f"{base_url}/api/{uid}/graphql",
                json={
                    "query": (
                        f"{{ entries(field: {field_enum}, query: \"{query_token}\", "
                        f"queryType: {query_enum}, size: {graphql_size}) {{ id {field} }} }}"
                    )
                },
                timeout=10,
            ).json()
            data_entries = entries_query.get("data", {}).get("entries", [])
            entries_query["data"] = {"entries": normalise_entries(data_entries)}
            save_json(output_dir / "graphql" / f"{uid}_entries.json", entries_query)


def main() -> None:
    args = parse_args()
    output_dir = Path(args.output_dir)
    reset_indices(args.elastic_url, args.indices)
    proc = start_service(args.command)
    try:
        wait_for_service(args.base_url, args.wait_seconds)
        capture_artifacts(
            base_url=args.base_url,
            output_dir=output_dir,
            query_token=args.query_token,
            graphql_size=args.graphql_size,
            skip_openapi=args.skip_openapi,
            skip_graphql_introspection=args.skip_graphql_introspection,
        )
    finally:
        stop_service(proc)


if __name__ == "__main__":
    main()
