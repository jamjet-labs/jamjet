"""
Auth hooks for an engine behind a platform that checks the caller's identity.

``TokenSourceAuth`` puts a token from any source in a header of its own, on
requests to one origin. ``GoogleIdTokenAuth`` gets a Google ID token through
google-auth (``pip install 'jamjet[google-auth]'``). Pass either as
``JamjetClient(auth=...)``; the JamJet token stays in ``Authorization``.
"""

from jamjet.auth.google import GoogleIdTokenAuth
from jamjet.auth.token_source import IdentityTokenError, TokenSourceAuth

__all__ = ["GoogleIdTokenAuth", "IdentityTokenError", "TokenSourceAuth"]
