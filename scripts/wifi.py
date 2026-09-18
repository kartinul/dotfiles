#!/usr/local/bin/python3

import os
import sys
import time
import html
import requests
from io import BytesIO
import xml.etree.ElementTree as ET


class Sophos():
    GATEWAY = "http://172.16.68.6:8090/"
    LOGIN_LINK = "login.xml"
    LOGOUT_LINK = "logout.xml"

    def __getmilliepoch(self):
        return str(int(time.time() * 1000))

    def test(self, timeout: float = 1.5) -> bool:
        try:
            requests.get(self.GATEWAY, timeout=timeout)
            return True
        except requests.RequestException:
            return False

    def login(self, user: str, pswd: str) -> str:
        LINK = self.GATEWAY + self.LOGIN_LINK
        data = {
            "mode": "191",
            "username": user,
            "password": pswd,
            "a": self.__getmilliepoch(),
            "producttype": "0"
        }
        resp = requests.post(LINK, data=data, timeout=5)
        return self.get_message(resp.content).format(username=user)

    def logout(self, user: str) -> str:
        LINK = self.GATEWAY + self.LOGOUT_LINK
        data = {
            "mode": "193",
            "username": user,
            "a": self.__getmilliepoch(),
            "producttype": "0"
        }
        resp = requests.post(LINK, data=data, timeout=5)
        return self.get_message(resp.content)

    def get_message(self, response):
        f = BytesIO(response)
        tree = ET.parse(f)
        root = tree.getroot()
        return html.unescape(root.find("./message").text)


class Sophos128(Sophos):
    GATEWAY = "http://172.16.120.10:8090/"


if __name__ == "__main__":
    branches = [
        (Sophos(),    os.getenv("SOPHOS_USERNAME", "").split(","),    os.getenv("SOPHOS_PASSWORD", "").split(",")),
        (Sophos128(), os.getenv("SOPHOS128_USERNAME", "").split(","), os.getenv("SOPHOS128_PASSWORD", "").split(",")),
    ]

    gateway = None
    credentials = None
    for gw, users, pswds in branches:
        if gw.test():
            gateway = gw
            credentials = (users, pswds)
            break

    if gateway is None:
        print("✗ No known gateway reachable — not on a recognized branch network")
        sys.exit(0)

    SOPHOS_USERNAME, SOPHOS_PASSWORD = credentials

    for user in SOPHOS_USERNAME:
        gateway.logout(user)

    if len(sys.argv) > 1 and "logout".startswith(sys.argv[1].lower()):
        print("✓ Logout successful")
        sys.exit(0)

    failed = []
    logged_in = None

    for user, pswd in zip(SOPHOS_USERNAME, SOPHOS_PASSWORD):
        try:
            msg = gateway.login(user, pswd)
            if msg.lower().startswith("you are signed in as"):
                logged_in = user
                break
            failed.append(f"✗ {user}: {msg}")
        except Exception as e:
            failed.append(f"✗ {user}: {e}")

    if logged_in:
        print(f"✓ Logged in as {logged_in}")
    else:
        print("\n".join(failed))
