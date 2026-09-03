#!/usr/local/bin/python3

import os
import sys
import time
import html
import requests
from io import BytesIO
import xml.etree.ElementTree as ET 

class Sophos():
    def __init__(self):
        self.GATEWAY = "http://172.16.68.6:8090/"
        self.LOGIN_LINK = "login.xml"
        self.LOGOUT_LINK = "logout.xml"
    
    def __getmilliepoch(self):
        return str(int(time.time()*1000))

    def login(self, user: str, pswd: str) -> str:
        LINK = self.GATEWAY + self.LOGIN_LINK
        data = {
                "mode": "191",
                "username": user,
                "password": pswd,
                "a": self.__getmilliepoch(),
                "producttype": "0"
        }

        resp = requests.post(LINK, data=data)
        return self.get_message(resp.content).format(username=user)

    def logout(self, user: str) -> str:
        LINK = self.GATEWAY + self.LOGOUT_LINK
        data = {
                "mode": "193",
                "username": user,
                "a": self.__getmilliepoch(),
                "producttype": "0"
        }

        resp = requests.post(LINK, data=data)
        return self.get_message(resp.content)

    def get_message(self, response):
        f = BytesIO(response)
        tree = ET.parse(f)

        root = tree.getroot()
        return html.unescape(root.find("./message").text)

if __name__ == "__main__":
    SOPHOS_USERNAME = os.getenv("SOPHOS_USERNAME").split(',')
    SOPHOS_PASSWORD = os.getenv("SOPHOS_PASSWORD").split(',')

    s = Sophos()

    for user in SOPHOS_USERNAME:
        res = s.logout(user)
    if (len(sys.argv) > 1 and ("logout".startswith(sys.argv[1]))):
        print(f"✓ Logout successful")
        exit()
    for user,pswd in list(zip(SOPHOS_USERNAME,SOPHOS_PASSWORD)):
        try:
            msg = s.login(user, pswd)
            if (msg.lower().startswith("You are signed in as".lower())):
                print(f"✓ {user}: Login successful")
                break
            else:
                print(f"✗ {user}: {msg}")
        except Exception as e:
            print(f"Failed to login with {user}")
            raise e