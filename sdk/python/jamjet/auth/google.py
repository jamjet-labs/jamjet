"""
A Google ID token for an engine behind a platform that checks one.

``GoogleIdTokenAuth`` gets the token through google-auth, from the metadata
server or from the service account file that ``GOOGLE_APPLICATION_CREDENTIALS``
names. It needs ``pip install 'jamjet[google-auth]'``.
"""

from __future__ import annotations

import ipaddress

import httpx

from jamjet.auth.token_source import TokenSourceAuth


def _is_loopback(host: str) -> bool:
    if host.lower() == "localhost":
        return True
    try:
        return ipaddress.ip_address(host).is_loopback
    except ValueError:
        return False


class GoogleIdTokenAuth(TokenSourceAuth):
    """A Google ID token for ``audience``, in ``header``, on every request to ``origin``.

    ``origin`` defaults to the audience's own, so for a service whose audience
    is its URL, pass that URL::

        auth = GoogleIdTokenAuth("https://engine.example")
        client = JamjetClient("https://engine.example", api_token=jamjet_token, auth=auth)

    The token goes over https only, unless the host is loopback. It is fetched
    with ``google.oauth2.id_token.fetch_id_token`` on a worker thread, through
    google-auth's own transport, so nothing the client sends reaches the
    metadata server. A ``google.auth.exceptions.TransportError`` is tried
    twice more; a ``RefreshError`` or a ``DefaultCredentialsError`` (no
    credentials here, or none that can mint an ID token) fails at once.
    Caching, refresh and the one resend after a platform 401 are as in
    ``TokenSourceAuth``.
    """

    def __init__(self, audience: str, *, header: str = "X-Serverless-Authorization", origin: str | None = None) -> None:
        try:
            import google.auth.exceptions
            import google.auth.transport.requests
            import google.oauth2.id_token
        except ImportError as exc:
            raise ImportError(
                "GoogleIdTokenAuth needs google-auth with its requests transport: pip install 'jamjet[google-auth]'"
            ) from exc
        if not isinstance(audience, str) or not audience.strip():
            raise ValueError("audience must be the audience the ID token is for, such as the service's URL")
        if origin is None:
            url = httpx.URL(audience)
            if url.scheme not in ("http", "https") or not url.host:
                raise ValueError(
                    "The audience is not an http or https URL, so the origin the token goes to cannot come from it. "
                    "Pass origin=, the engine's URL."
                )
            origin = audience
        target = httpx.URL(origin)
        if target.scheme != "https" and not _is_loopback(target.host):
            raise ValueError(
                f"An ID token goes over https only, unless the host is loopback. {origin} is neither, "
                "so the token would cross the network in the clear."
            )

        def fetch() -> str:
            request = google.auth.transport.requests.Request()
            token: str = google.oauth2.id_token.fetch_id_token(request, audience)  # type: ignore[no-untyped-call]
            return token

        super().__init__(fetch, origin=origin, header=header)
        self._retry_on = (*self._retry_on, google.auth.exceptions.TransportError)
        self._fail_on = (google.auth.exceptions.RefreshError, google.auth.exceptions.DefaultCredentialsError)
        self._audience = audience

    @property
    def audience(self) -> str:
        """The audience the ID token is for."""
        return self._audience
