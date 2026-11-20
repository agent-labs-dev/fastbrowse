import contextlib
import subprocess
import time

import uno
from com.sun.star.beans import PropertyValue


def pv(n, v):
    p = PropertyValue()
    p.Name = n
    p.Value = v
    return p


class Office:
    def __init__(self, profile="/tmp/lo-build", port=2002):
        self.proc = subprocess.Popen(
            [
                "soffice",
                "--headless",
                "--norestore",
                f"-env:UserInstallation=file://{profile}",
                f"--accept=socket,host=localhost,port={port};urp;",
            ]
        )
        local = uno.getComponentContext()
        resolver = local.ServiceManager.createInstanceWithContext("com.sun.star.bridge.UnoUrlResolver", local)
        for _ in range(90):
            try:
                self.ctx = resolver.resolve(f"uno:socket,host=localhost,port={port};urp;StarOffice.ComponentContext")
                break
            except Exception:
                time.sleep(1)
        self.smgr = self.ctx.ServiceManager
        self.desktop = self.smgr.createInstanceWithContext("com.sun.star.frame.Desktop", self.ctx)
        self.disp = self.smgr.createInstanceWithContext("com.sun.star.frame.DispatchHelper", self.ctx)

    def load(self, url, *props):
        return self.desktop.loadComponentFromURL(url, "_blank", 0, (pv("Hidden", True), *props))

    def dispatch(self, doc, command):
        self.disp.executeDispatch(doc.CurrentController.Frame, command, "", 0, ())

    def close(self):
        with contextlib.suppress(Exception):
            self.desktop.terminate()
        self.proc.wait(30)
