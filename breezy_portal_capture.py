#!/usr/bin/env /usr/bin/python3
"""
breezy_portal_capture — PipeWire ScreenCast capture daemon for breezy-cosmic.

Uses xdg-desktop-portal ScreenCast to capture the primary monitor and writes
raw video frames to shared memory for the Rust renderer to consume.

Shared memory layout at /dev/shm/breezy_capture:
  Bytes 0-3:    magic  0xBCAPu32
  Bytes 4-7:    width  u32 LE
  Bytes 8-11:   height u32 LE
  Bytes 12-15:  stride u32 LE  (bytes per row)
  Bytes 16-19:  format u32 LE  (0=BGRx, 1=RGBx, 2=BGRA, 3=RGBA)
  Bytes 20-23:  frame_seq u32 LE (increments each new frame)
  Bytes 24-31:  timestamp_ns u64 LE (monotonic clock)
  Bytes 32+:    raw pixel data (height * stride bytes)
"""

import sys
import os
import struct
import signal
import time
import mmap

import dbus
import dbus.mainloop.glib

import gi
gi.require_version('Gst', '1.0')
gi.require_version('GstVideo', '1.0')
from gi.repository import Gst, GLib

# ── Constants ──────────────────────────────────────────────────────────────────
SHM_PATH = "/dev/shm/breezy_capture"
HEADER_SIZE = 32
MAGIC = 0x42434150  # 'BCAP'

# ── Globals ────────────────────────────────────────────────────────────────────
pipeline = None
loop = None
shm_mm = None
frame_seq = 0
target_output = None


def init_shm(width, height, stride, fmt=0):
    """Create or resize the shared memory region."""
    global shm_mm

    total_size = HEADER_SIZE + height * stride

    fd = os.open(SHM_PATH, os.O_CREAT | os.O_RDWR, 0o666)
    os.ftruncate(fd, total_size)

    shm_mm = mmap.mmap(fd, total_size)
    os.close(fd)

    # Write header
    shm_mm[0:4] = struct.pack('<I', MAGIC)
    shm_mm[4:8] = struct.pack('<I', width)
    shm_mm[8:12] = struct.pack('<I', height)
    shm_mm[12:16] = struct.pack('<I', stride)
    shm_mm[16:20] = struct.pack('<I', fmt)
    shm_mm[20:24] = struct.pack('<I', 0)
    shm_mm[24:32] = struct.pack('<Q', 0)

    print(f"[capture] SHM initialized: {width}x{height} stride={stride} total={total_size} bytes",
          flush=True)


def write_frame(data, width, height, stride):
    """Write a frame to shared memory."""
    global frame_seq, shm_mm

    if shm_mm is None:
        return

    frame_seq += 1
    ts = time.clock_gettime_ns(time.CLOCK_MONOTONIC)

    payload_size = height * stride
    if len(data) < payload_size:
        if frame_seq <= 3:
            print(f"[capture] WARNING: frame data too short ({len(data)} < {payload_size})",
                  flush=True)
        return

    # Write pixel data first, then update header atomically (seq last)
    shm_mm[HEADER_SIZE:HEADER_SIZE + payload_size] = data[:payload_size]
    shm_mm[24:32] = struct.pack('<Q', ts)
    shm_mm[20:24] = struct.pack('<I', frame_seq)

    if frame_seq % 300 == 0:
        print(f"[capture] frame {frame_seq}, {width}x{height}", flush=True)


# ── GStreamer appsink callback ─────────────────────────────────────────────────
def on_new_sample(appsink):
    """Called by GStreamer when a new frame arrives from PipeWire."""
    sample = appsink.emit('pull-sample')
    if sample is None:
        return Gst.FlowReturn.OK

    buf = sample.get_buffer()
    caps = sample.get_caps()
    struct_ = caps.get_structure(0)

    width = struct_.get_value('width')
    height = struct_.get_value('height')

    success, map_info = buf.map(Gst.MapFlags.READ)
    if not success:
        return Gst.FlowReturn.ERROR

    stride = width * 4  # BGRx = 4 bytes per pixel
    data = bytes(map_info.data)
    buf.unmap(map_info)

    if shm_mm is None:
        init_shm(width, height, stride, fmt=0)

    write_frame(data, width, height, stride)
    return Gst.FlowReturn.OK


# ── Portal ScreenCast ─────────────────────────────────────────────────────────

class ScreenCastPortal:
    """Manages portal ScreenCast session using GLib main loop for D-Bus signals."""

    def __init__(self, bus, mainloop):
        self.bus = bus
        self.mainloop = mainloop
        self.portal = bus.get_object(
            'org.freedesktop.portal.Desktop',
            '/org/freedesktop/portal/desktop'
        )
        self.screencast = dbus.Interface(
            self.portal, 'org.freedesktop.portal.ScreenCast'
        )
        self.session_handle = None
        self.pw_node_id = None
        self.source_type = "monitor"  # "monitor" or "window"
        self._counter = 0

    def _token(self):
        self._counter += 1
        return f"breezy_{os.getpid()}_{self._counter}"

    def _sender_name(self):
        return self.bus.get_unique_name().replace('.', '_').lstrip(':')

    def start(self):
        """Run the full portal sequence: CreateSession → SelectSources → Start."""
        self._create_session()

    def _create_session(self):
        token = self._token()
        session_token = self._token()
        request_path = f"/org/freedesktop/portal/desktop/request/{self._sender_name()}/{token}"

        print(f"[capture] Creating session (request: {request_path})...", flush=True)

        self.bus.add_signal_receiver(
            self._on_create_session_response,
            signal_name='Response',
            dbus_interface='org.freedesktop.portal.Request',
            path=request_path,
        )

        self.screencast.CreateSession(
            dbus.Dictionary({
                'handle_token': token,
                'session_handle_token': session_token,
            }, signature='sv')
        )

    def _on_create_session_response(self, response, results):
        if response != 0:
            print(f"[capture] ERROR: CreateSession failed (code {response})", flush=True)
            self.mainloop.quit()
            return

        self.session_handle = str(results.get('session_handle', ''))
        print(f"[capture] Session created: {self.session_handle}", flush=True)
        self._select_sources()

    def _select_sources(self):
        token = self._token()
        request_path = f"/org/freedesktop/portal/desktop/request/{self._sender_name()}/{token}"

        # Always request BOTH source types (MONITOR | WINDOW = 3) so the
        # portal dialog shows all tabs.  COSMIC's xdg-desktop-portal may
        # not display screens at all when only type 1 is requested.
        src_types = 3  # MONITOR | WINDOW — dialog will show both tabs
        print(f"[capture] Selecting sources (requested: {self.source_type}, "
              f"portal types={src_types}, cursor embedded)...", flush=True)

        self.bus.add_signal_receiver(
            self._on_select_sources_response,
            signal_name='Response',
            dbus_interface='org.freedesktop.portal.Request',
            path=request_path,
        )

        # Don't use persist_mode — a cached restore token from a previous
        # session might force the wrong source type.
        self.screencast.SelectSources(
            dbus.ObjectPath(self.session_handle),
            dbus.Dictionary({
                'handle_token': token,
                'types': dbus.UInt32(src_types),
                'cursor_mode': dbus.UInt32(2),   # EMBEDDED
            }, signature='sv')
        )

    def _on_select_sources_response(self, response, results):
        if response != 0:
            print(f"[capture] ERROR: SelectSources failed (code {response})", flush=True)
            self.mainloop.quit()
            return

        print("[capture] Sources selected", flush=True)
        self._start_session()

    def _start_session(self):
        token = self._token()
        request_path = f"/org/freedesktop/portal/desktop/request/{self._sender_name()}/{token}"

        print("[capture] Starting ScreenCast (dialog may appear)...", flush=True)

        self.bus.add_signal_receiver(
            self._on_start_response,
            signal_name='Response',
            dbus_interface='org.freedesktop.portal.Request',
            path=request_path,
        )

        self.screencast.Start(
            dbus.ObjectPath(self.session_handle),
            '',  # parent_window
            dbus.Dictionary({
                'handle_token': token,
            }, signature='sv')
        )

    def _on_start_response(self, response, results):
        if response != 0:
            if response == 1:
                print("[capture] ERROR: User cancelled the ScreenCast dialog", flush=True)
            else:
                print(f"[capture] ERROR: Start failed (code {response})", flush=True)
            self.mainloop.quit()
            return

        streams = results.get('streams', [])
        if not streams:
            print("[capture] ERROR: No streams returned", flush=True)
            self.mainloop.quit()
            return

        node_id, props = streams[0]
        self.pw_node_id = int(node_id)

        prop_dict = dict(props)
        src_type = prop_dict.get('source_type', 'unknown')
        size = prop_dict.get('size', (0, 0))
        print(f"[capture] Stream ready: PipeWire node={self.pw_node_id}, "
              f"type={src_type}, size={size}", flush=True)

        # Now launch GStreamer pipeline
        self._start_gstreamer()

    def _start_gstreamer(self):
        """Build and start the GStreamer pipeline targeting the PipeWire node."""
        global pipeline

        pipeline_str = (
            f'pipewiresrc path={self.pw_node_id} do-timestamp=true '
            f'keepalive-time=1000 resend-last=true min-buffers=2 ! '
            f'videoconvert ! '
            f'video/x-raw,format=BGRx ! '
            f'appsink name=sink emit-signals=true sync=false max-buffers=2 drop=true'
        )

        print(f"[capture] Pipeline: {pipeline_str}", flush=True)
        pipeline = Gst.parse_launch(pipeline_str)

        appsink = pipeline.get_by_name('sink')
        appsink.connect('new-sample', on_new_sample)

        # Bus messages
        gst_bus = pipeline.get_bus()
        gst_bus.add_signal_watch()
        gst_bus.connect('message', self._on_bus_message)

        ret = pipeline.set_state(Gst.State.PLAYING)
        if ret == Gst.StateChangeReturn.FAILURE:
            print("[capture] ERROR: Failed to start GStreamer pipeline", flush=True)
            self.mainloop.quit()
            return

        print("[capture] GStreamer pipeline running — frames going to SHM", flush=True)

    def _on_bus_message(self, bus, message):
        mtype = message.type
        if mtype == Gst.MessageType.EOS:
            print("[capture] End of stream", flush=True)
            self.mainloop.quit()
        elif mtype == Gst.MessageType.ERROR:
            err, debug = message.parse_error()
            print(f"[capture] GStreamer ERROR: {err.message}", flush=True)
            if debug:
                print(f"[capture] Debug: {debug}", flush=True)
            self.mainloop.quit()
        elif mtype == Gst.MessageType.STATE_CHANGED:
            if message.src == pipeline:
                old, new, pending = message.parse_state_changed()
                print(f"[capture] Pipeline: {old.value_nick} -> {new.value_nick}", flush=True)
        return True


def cleanup(*args):
    """Clean up on exit."""
    global pipeline, loop, shm_mm
    print("[capture] Shutting down...", flush=True)
    if pipeline:
        pipeline.set_state(Gst.State.NULL)
    if shm_mm:
        shm_mm.close()
    try:
        os.unlink(SHM_PATH)
    except OSError:
        pass
    if loop and loop.is_running():
        loop.quit()
    sys.exit(0)


def main():
    global loop, target_output

    source_type = "monitor"  # default
    if len(sys.argv) > 1:
        target_output = sys.argv[1]
        print(f"[capture] Target output: {target_output}", flush=True)
    if len(sys.argv) > 2:
        source_type = sys.argv[2].lower()
        print(f"[capture] Source type: {source_type}", flush=True)

    # CRITICAL: Set up GLib as the D-Bus main loop BEFORE creating the bus
    dbus.mainloop.glib.DBusGMainLoop(set_as_default=True)

    # Initialize GStreamer
    Gst.init(None)

    # Signal handlers
    signal.signal(signal.SIGTERM, cleanup)
    signal.signal(signal.SIGINT, cleanup)

    # Session bus (GLib main loop will handle signals)
    bus = dbus.SessionBus()

    # Create the GLib main loop
    loop = GLib.MainLoop()

    # Start the portal session chain
    portal = ScreenCastPortal(bus, loop)
    portal.source_type = source_type
    portal.start()

    # Run the main loop — this processes both D-Bus signals and GStreamer
    print("[capture] Entering main loop...", flush=True)
    try:
        loop.run()
    except KeyboardInterrupt:
        pass
    finally:
        cleanup()


if __name__ == '__main__':
    main()
