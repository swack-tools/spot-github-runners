"""Opaque-box HTTP test client for Edge Worker E2E testing."""

from __future__ import annotations

import json
import time
import urllib.error
import urllib.parse
import urllib.request
from typing import Any

from fixtures import (
    COOKIE_NAME,
    DEFAULT_TEST_SECRET,
    SWACKHAMER_LOGIN,
    SWACKHAMER_USER_ID,
    create_session_token,
)


class HttpResponse:
    """Convenience response wrapper for HTTP assertions."""

    def __init__(
        self,
        status_code: int,
        headers: dict[str, str],
        body: bytes,
        cookies: dict[str, str],
    ):
        self.status_code = status_code
        self.headers = headers
        self.body = body
        self.cookies = cookies
        self._text: str | None = None
        self._json: Any = None

    @property
    def text(self) -> str:
        if self._text is None:
            self._text = self.body.decode("utf-8", errors="replace")
        return self._text

    @property
    def json(self) -> Any:
        if self._json is None:
            self._json = json.loads(self.text)
        return self._json

    def header(self, name: str) -> str | None:
        name_lower = name.lower()
        for k, v in self.headers.items():
            if k.lower() == name_lower:
                return v
        return None

    def assert_status(self, expected_status: int) -> HttpResponse:
        assert self.status_code == expected_status, (
            f"Expected HTTP {expected_status}, got {self.status_code}. Body: {self.text[:500]}"
        )
        return self


class NoRedirectHandler(urllib.request.HTTPRedirectHandler):
    """Handler that stops redirects to inspect 301/302 responses directly."""

    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


class EdgeTestClient:
    """HTTP client communicating exclusively over standard HTTP interfaces."""

    def __init__(self, base_url: str, secret: str = DEFAULT_TEST_SECRET):
        self.base_url = base_url.rstrip("/")
        self.secret = secret
        self.cookies: dict[str, str] = {}

    def set_cookie(self, name: str, value: str) -> None:
        self.cookies[name] = value

    def clear_cookies(self) -> None:
        self.cookies.clear()

    def login_as_owner(self, exp_offset: int = 604800, iat: int | None = None) -> str:
        """Mint and set session cookie for owner swackhamer."""
        now = int(time.time())
        issued = now if iat is None else iat
        payload = {
            "sub": str(SWACKHAMER_USER_ID),
            "login": SWACKHAMER_LOGIN,
            "role": "owner",
            "iat": issued,
            "exp": issued + exp_offset,
        }
        token = create_session_token(payload, secret=self.secret)
        self.set_cookie(COOKIE_NAME, token)
        return token

    def login_as_admin(
        self,
        user_id: int = 999999,
        login: str = "test-admin",
        exp_offset: int = 604800,
        iat: int | None = None,
    ) -> str:
        """Mint and set session cookie for an added admin."""
        now = int(time.time())
        issued = now if iat is None else iat
        payload = {
            "sub": str(user_id),
            "login": login,
            "role": "admin",
            "iat": issued,
            "exp": issued + exp_offset,
        }
        token = create_session_token(payload, secret=self.secret)
        self.set_cookie(COOKIE_NAME, token)
        return token

    def login_as_member(
        self,
        user_id: int = 888888,
        login: str = "regular-user",
        exp_offset: int = 604800,
        iat: int | None = None,
    ) -> str:
        """Mint and set session cookie for a non-admin member."""
        now = int(time.time())
        issued = now if iat is None else iat
        payload = {
            "sub": str(user_id),
            "login": login,
            "role": "member",
            "iat": issued,
            "exp": issued + exp_offset,
        }
        token = create_session_token(payload, secret=self.secret)
        self.set_cookie(COOKIE_NAME, token)
        return token

    def request(
        self,
        method: str,
        path: str,
        json_data: Any = None,
        data: bytes | None = None,
        headers: dict[str, str] | None = None,
        follow_redirects: bool = False,
    ) -> HttpResponse:
        url = f"{self.base_url}{path}" if path.startswith("/") else f"{self.base_url}/{path}"
        req_headers = dict(headers or {})

        # Prepare request body
        body_bytes = None
        if json_data is not None:
            body_bytes = json.dumps(json_data).encode("utf-8")
            req_headers["Content-Type"] = "application/json; charset=utf-8"
        elif data is not None:
            body_bytes = data

        # Add cookie header
        if self.cookies:
            cookie_str = "; ".join(f"{k}={v}" for k, v in self.cookies.items())
            req_headers["Cookie"] = cookie_str
        req_headers.setdefault("Connection", "close")

        req = urllib.request.Request(url, data=body_bytes, headers=req_headers, method=method)

        handlers: list[Any] = []
        if not follow_redirects:
            handlers.append(NoRedirectHandler())
        opener = urllib.request.build_opener(*handlers)

        try:
            resp = opener.open(req, timeout=10.0)
            status_code = resp.status
            resp_headers = dict(resp.headers.items())
            resp_body = resp.read()
            resp.close()
        except urllib.error.HTTPError as err:
            status_code = err.code
            resp_headers = dict(err.headers.items())
            resp_body = err.read()
            err.close()

        # Update cookies from response Set-Cookie
        received_cookies: dict[str, str] = {}
        for k, v in resp_headers.items():
            if k.lower() == "set-cookie":
                cookie_parts = v.split(";", 1)[0].split("=", 1)
                if len(cookie_parts) == 2:
                    c_name, c_val = cookie_parts[0].strip(), cookie_parts[1].strip()
                    received_cookies[c_name] = c_val
                    self.cookies[c_name] = c_val

        return HttpResponse(
            status_code=status_code,
            headers=resp_headers,
            body=resp_body,
            cookies=received_cookies,
        )

    def get(self, path: str, headers: dict[str, str] | None = None, follow_redirects: bool = False) -> HttpResponse:
        return self.request("GET", path, headers=headers, follow_redirects=follow_redirects)

    def post(
        self,
        path: str,
        json_data: Any = None,
        data: bytes | None = None,
        headers: dict[str, str] | None = None,
        follow_redirects: bool = False,
    ) -> HttpResponse:
        return self.request(
            "POST",
            path,
            json_data=json_data,
            data=data,
            headers=headers,
            follow_redirects=follow_redirects,
        )

    def delete(self, path: str, headers: dict[str, str] | None = None) -> HttpResponse:
        return self.request("DELETE", path, headers=headers)
