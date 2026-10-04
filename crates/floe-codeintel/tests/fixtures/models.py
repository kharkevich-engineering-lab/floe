"""Models (fixture)."""
import json

VERSION = "1.0"


class User:
    """A user."""

    def __init__(self, name):
        self.name = name

    def to_json(self):
        return json.dumps({"name": self.name})

    def _secret(self):
        def inner():
            return 1

        return inner()


def load(text):
    data = json.loads(text)
    return User(data["name"])


def test_load():
    assert load('{"name": "a"}').name == "a"
