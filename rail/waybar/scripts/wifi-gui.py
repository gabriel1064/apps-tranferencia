#!/usr/bin/env python3
"""Gerenciador Wi‑Fi gráfico (GTK4) para iwd — sem iwctl interativo."""
from __future__ import annotations

import sys
import threading

import dbus
import dbus.mainloop.glib
import dbus.service
from gi.repository import GLib, Gtk

IWD = "net.connman.iwd"
OM = "org.freedesktop.DBus.ObjectManager"
PROPS = "org.freedesktop.DBus.Properties"
STATION = "net.connman.iwd.Station"
NETWORK = "net.connman.iwd.Network"
DEVICE = "net.connman.iwd.Device"
AGENT = "net.connman.iwd.Agent"
AGENT_MGR = "net.connman.iwd.AgentManager"

AGENT_PATH = "/com/quickshell/wifi/agent"


def signal_bars(mbm: int) -> str:
    # GetOrderedNetworks retorna sinal em mBm (ex.: -5500 ≈ -55 dBm)
    dbm = mbm / 100.0
    if dbm >= -50:
        return "▂▄▆█"
    if dbm >= -60:
        return "▂▄▆_"
    if dbm >= -70:
        return "▂▄__"
    if dbm >= -80:
        return "▂___"
    return "____"


class PassphraseAgent(dbus.service.Object):
    def __init__(self, bus: dbus.SystemBus, window: "WifiWindow"):
        dbus.service.Object.__init__(self, bus, AGENT_PATH)
        self.window = window
        self._result: str | None = None
        self._event = threading.Event()

    def ask(self, title: str, network_name: str) -> str:
        self._result = None
        self._event.clear()

        def show() -> None:
            dialog = Gtk.AlertDialog(
                modal=True,
                message=title,
                detail=f"Rede: {network_name}",
                buttons=["Cancelar", "Conectar"],
            )
            # Gtk.AlertDialog não tem entry — usa PasswordEntryDialog simples
            box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=12)
            box.set_margin_top(16)
            box.set_margin_bottom(16)
            box.set_margin_start(16)
            box.set_margin_end(16)
            label = Gtk.Label(label=f"{title}\n{network_name}", xalign=0)
            entry = Gtk.PasswordEntry(show_peek_icon=True)
            entry.set_activates_default(True)
            box.append(label)
            box.append(entry)

            win = Gtk.Window(title="Senha Wi‑Fi", transient_for=self.window, modal=True)
            win.set_default_size(360, 140)
            win.set_child(box)

            btn_row = Gtk.Box(spacing=8, halign=Gtk.Align.END)
            cancel = Gtk.Button(label="Cancelar")
            ok = Gtk.Button(label="Conectar", css_classes=["suggested-action"])
            btn_row.append(cancel)
            btn_row.append(ok)
            box.append(btn_row)

            def finish(value: str | None) -> None:
                self._result = value
                win.destroy()
                self._event.set()

            cancel.connect("clicked", lambda *_: finish(None))
            ok.connect("clicked", lambda *_: finish(entry.get_text() or None))
            entry.connect("activate", lambda *_: finish(entry.get_text() or None))
            win.present()
            entry.grab_focus()

        GLib.idle_add(show)
        self._event.wait(timeout=120)
        if not self._result:
            raise dbus.exceptions.DBusException(
                "net.connman.iwd.Agent.Error.Canceled", "canceled"
            )
        return self._result

    @dbus.service.method(AGENT, in_signature="", out_signature="")
    def Release(self) -> None:
        pass

    @dbus.service.method(AGENT, in_signature="o", out_signature="s")
    def RequestPassphrase(self, network: str) -> str:
        name = self.window.network_name(network) or network
        return self.ask("Digite a senha do Wi‑Fi", name)

    @dbus.service.method(AGENT, in_signature="o", out_signature="s")
    def RequestPrivateKeyPassphrase(self, network: str) -> str:
        name = self.window.network_name(network) or network
        return self.ask("Senha da chave privada", name)

    @dbus.service.method(AGENT, in_signature="o", out_signature="ss")
    def RequestUserNameAndPassword(self, network: str) -> tuple[str, str]:
        raise dbus.exceptions.DBusException(
            "net.connman.iwd.Agent.Error.Canceled", "unsupported"
        )

    @dbus.service.method(AGENT, in_signature="os", out_signature="s")
    def RequestUserPassword(self, network: str, user: str) -> str:
        raise dbus.exceptions.DBusException(
            "net.connman.iwd.Agent.Error.Canceled", "unsupported"
        )

    @dbus.service.method(AGENT, in_signature="s", out_signature="")
    def Cancel(self, reason: str) -> None:
        self._result = None
        self._event.set()


class WifiWindow(Gtk.ApplicationWindow):
    def __init__(self, app: Gtk.Application, bus: dbus.SystemBus):
        super().__init__(application=app, title="Wi‑Fi")
        self.set_default_size(420, 520)
        self.bus = bus
        self.station_path: str | None = None
        self.device_name = "wlan0"
        self._rows: list[tuple[str, int, bool, str]] = []
        self._busy = False

        self.status = Gtk.Label(xalign=0, wrap=True)
        self.status.add_css_class("dim-label")

        self.listbox = Gtk.ListBox(selection_mode=Gtk.SelectionMode.SINGLE)
        self.listbox.set_hexpand(True)
        self.listbox.set_vexpand(True)
        self.listbox.connect("row-activated", self.on_activate)

        scrolled = Gtk.ScrolledWindow(child=self.listbox)
        scrolled.set_policy(Gtk.PolicyType.NEVER, Gtk.PolicyType.AUTOMATIC)

        scan_btn = Gtk.Button(label="Atualizar")
        scan_btn.connect("clicked", lambda *_: self.refresh(scan=True))
        disc_btn = Gtk.Button(label="Desconectar")
        disc_btn.connect("clicked", lambda *_: self.disconnect())
        conn_btn = Gtk.Button(label="Conectar", css_classes=["suggested-action"])
        conn_btn.connect("clicked", lambda *_: self.connect_selected())

        buttons = Gtk.Box(spacing=8)
        buttons.append(scan_btn)
        buttons.append(disc_btn)
        buttons.append(conn_btn)

        root = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=10)
        root.set_margin_top(12)
        root.set_margin_bottom(12)
        root.set_margin_start(12)
        root.set_margin_end(12)
        root.append(self.status)
        root.append(scrolled)
        root.append(buttons)
        self.set_child(root)

        self.agent = PassphraseAgent(bus, self)
        try:
            mgr = dbus.Interface(bus.get_object(IWD, "/net/connman/iwd"), AGENT_MGR)
            mgr.RegisterAgent(AGENT_PATH)
        except dbus.DBusException as exc:
            self.set_status(f"Aviso: agent iwd não registrado ({exc.get_dbus_message()})")

        self.find_station()
        self.refresh(scan=True)

    def set_status(self, text: str) -> None:
        self.status.set_text(text)

    def find_station(self) -> None:
        om = dbus.Interface(self.bus.get_object(IWD, "/"), OM)
        for path, ifaces in om.GetManagedObjects().items():
            if STATION in ifaces:
                self.station_path = str(path)
                try:
                    props = dbus.Interface(self.bus.get_object(IWD, path), PROPS)
                    self.device_name = str(props.Get(DEVICE, "Name"))
                except dbus.DBusException:
                    pass
                return
        raise RuntimeError("Nenhuma interface Wi‑Fi (iwd Station) encontrada")

    def station(self):
        assert self.station_path
        return dbus.Interface(self.bus.get_object(IWD, self.station_path), STATION)

    def props(self, path: str):
        return dbus.Interface(self.bus.get_object(IWD, path), PROPS)

    def network_name(self, path: str) -> str | None:
        try:
            return str(self.props(path).Get(NETWORK, "Name"))
        except dbus.DBusException:
            return None

    def refresh(self, scan: bool = False) -> None:
        if self._busy:
            return
        self._busy = True
        self.set_status("Procurando redes…" if scan else "Atualizando…")

        def work() -> None:
            err = None
            rows: list[tuple[str, int, bool, str]] = []
            state = "?"
            connected = ""
            try:
                st = self.station()
                props = self.props(self.station_path)  # type: ignore[arg-type]
                if scan:
                    try:
                        st.Scan()
                    except dbus.DBusException:
                        pass
                    import time

                    for _ in range(20):
                        scanning = bool(props.Get(STATION, "Scanning"))
                        if not scanning:
                            break
                        time.sleep(0.15)
                state = str(props.Get(STATION, "State"))
                try:
                    cpath = str(props.Get(STATION, "ConnectedNetwork"))
                    connected = self.network_name(cpath) or ""
                except dbus.DBusException:
                    connected = ""
                for path, signal in st.GetOrderedNetworks():
                    name = self.network_name(str(path)) or str(path)
                    is_conn = False
                    try:
                        is_conn = bool(self.props(str(path)).Get(NETWORK, "Connected"))
                    except dbus.DBusException:
                        pass
                    rows.append((str(path), int(signal), is_conn, name))
            except Exception as exc:  # noqa: BLE001
                err = str(exc)

            def apply() -> None:
                self._busy = False
                if err:
                    self.set_status(f"Erro: {err}")
                    return
                self._rows = rows
                while (child := self.listbox.get_first_child()) is not None:
                    self.listbox.remove(child)
                for path, signal, is_conn, name in rows:
                    mark = "● " if is_conn else "○ "
                    label = Gtk.Label(
                        label=f"{mark}{signal_bars(signal)}  {name}",
                        xalign=0,
                        hexpand=True,
                    )
                    row = Gtk.ListBoxRow()
                    row.set_child(label)
                    row._net_path = path  # type: ignore[attr-defined]
                    self.listbox.append(row)
                if connected:
                    self.set_status(f"{self.device_name}: {state} · {connected}")
                else:
                    self.set_status(f"{self.device_name}: {state}")

            GLib.idle_add(apply)

        threading.Thread(target=work, daemon=True).start()

    def selected_path(self) -> str | None:
        row = self.listbox.get_selected_row()
        if row is None:
            return None
        return getattr(row, "_net_path", None)

    def on_activate(self, *_args) -> None:
        self.connect_selected()

    def connect_selected(self) -> None:
        path = self.selected_path()
        if not path:
            self.set_status("Selecione uma rede")
            return
        name = self.network_name(path) or path
        self.set_status(f"Conectando a {name}…")

        def work() -> None:
            err = None
            try:
                net = dbus.Interface(self.bus.get_object(IWD, path), NETWORK)
                net.Connect()
            except dbus.DBusException as exc:
                err = exc.get_dbus_message() or str(exc)

            def done() -> None:
                if err:
                    self.set_status(f"Falha: {err}")
                else:
                    self.set_status(f"Conectado a {name}")
                self.refresh(scan=False)

            GLib.idle_add(done)

        threading.Thread(target=work, daemon=True).start()

    def disconnect(self) -> None:
        self.set_status("Desconectando…")

        def work() -> None:
            err = None
            try:
                self.station().Disconnect()
            except dbus.DBusException as exc:
                err = exc.get_dbus_message() or str(exc)

            def done() -> None:
                self.set_status(err or "Desconectado")
                self.refresh(scan=False)

            GLib.idle_add(done)

        threading.Thread(target=work, daemon=True).start()


class WifiApp(Gtk.Application):
    def __init__(self):
        super().__init__(application_id="com.quickshell.wifi")
        self.bus: dbus.SystemBus | None = None

    def do_activate(self) -> None:  # noqa: N802
        if self.bus is None:
            dbus.mainloop.glib.DBusGMainLoop(set_as_default=True)
            self.bus = dbus.SystemBus()
        win = WifiWindow(self, self.bus)
        win.present()


def main() -> int:
    return WifiApp().run(sys.argv)


if __name__ == "__main__":
    raise SystemExit(main())
