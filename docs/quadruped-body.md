# Quadruped Robot Body — Research & Design Notes

*Project: Give Claude wheels, eyes, and autonomy*
*Last updated: March 2026*

---

## Goals

1. Mobile robot body that Claude can control autonomously
2. Self-docking to charge (no human intervention required)
3. Hackable sensor/compute stack (cameras, depth, mic array)
4. Can go on car rides and walks
5. Budget target: under $3,500 all-in

---

## Platform Candidates

### Unitree Go2 Air — CURRENT FRONTRUNNER
- **Price:** ~$1,600 direct from China / ~$2,800 via US reseller
- **Battery:** 8000mAh / 236.8Wh, DC 29.6V nominal
- **Charger:** 33.6V 3.5A (~117W), ~2hr charge time
- **Runtime:** 1–2 hours
- **Weight:** ~15kg
- **IP rating:** IP54 (splash resistant, NOT rain-proof)
- **SDK:** unitree_legged_sdk for low-level motor control
- **Pros:** Proven platform, large community, good SDK, cameras built in
- **Cons:** No self-docking on Air/Pro (EDU only), IP54 limits outdoor use,
  US reseller markup is steep, different battery/charger from EDU tier
- **Self-docking:** Not supported natively — see DIY Magnetic Dock Design below

### Dobot Rover X1 — WATCH LIST
- **Price:** $949 early bird / $1,499 MSRP
- **Battery:** NOT DISCLOSED
- **Runtime:** NOT DISCLOSED
- **IP rating:** NOT DISCLOSED
- **SDK:** "Open source software package, customizable SDK/API"
- **Locomotion:** Wheel-leg hybrid (wheels for efficiency, legs for terrain)
- **Sold under:** INFFNI sub-brand of Dobot
- **Pros:** Wheel-leg hybrid is ideal for docking (precision approach on wheels),
  very affordable, Dobot is a legit company (IPO Dec 2024, 72k+ cobots shipped,
  Red Dot / iF Design Awards)
- **Cons:** Pre-launch, specs still hidden, international shipping TBD,
  no confirmed charging dock or power interface details
- **Status:** Monitor for spec release. If battery/charging details are good
  and there's an accessible power interface, this could leapfrog the Go2.

### Unitree Go2 EDU Plus — IDEAL BUT OVER BUDGET
- **Price:** $4,500+
- **Battery:** 15000mAh / 432Wh, DC 28.8V
- **Charger:** 33.6V 9A fast charge, ~1hr 15min
- **Self-docking:** YES, native support with self-charging board (~$350)
- **LiDAR:** Hesai XT16 or Mid360 for SLAM
- **Why it's the dream:** Everything just works. Self-docking, long runtime,
  full autonomy stack, LiDAR mapping.
- **Why we can't:** $4,500 + $350 charging board = $4,850 before sensors/compute

### XGO Mini 2 — PROTOTYPE PLATFORM
- **Price:** ~$500–750
- **Compute:** Raspberry Pi CM4
- **DOF:** 12–15
- **Pros:** Cheap, fully hackable, can prototype entire AI control stack
  (visual docking, voice control, navigation) before committing to bigger platform
- **Cons:** Tiny, not a real-world mobile agent platform
- **Use case:** Software development testbed. Everything built here transfers.

### DEEP Robotics Lite3 Basic — STRETCH GOAL
- **Price:** ~$2,890+ (EDU5OFF discount code may apply)
- **Pros:** Better build quality, IP-rated, good SDK
- **Cons:** At budget ceiling before any accessories

---

## DIY Magnetic Dock Design (for Go2 Air)

### The Problem
Unitree gates self-docking behind the EDU tier. The Air's charging port is
proprietary, and the EDU uses entirely different batteries/chargers. We can't
just buy the EDU charging board for the Air.

### The Solution
Sacrifice a second charger cable. The original charger plug stays permanently
in the robot's charging port. We splice the cable and insert a magnetic pogo
pin connector as a breakaway interface. The robot walks up to the dock, the
magnets snap together, power flows.

### Wiring Diagram

```
ROBOT SIDE:
┌──────────┐     ┌──────────┐     ┌──────────┐     ┌─────────────────┐
│ Go2 Air  │────▶│ OEM plug │────▶│ Polyfuse │────▶│ Magnetic pogo   │
│ charge   │     │ (stays   │     │ (5A re-  │     │ pins (robot half)│
│ port     │     │  plugged │     │  settable)│     │ EXPOSED - safe  │
│          │     │  in)     │     │          │     │ w/ fuse protect │
└──────────┘     └──────────┘     └──────────┘     └─────────────────┘

DOCK SIDE:
┌──────────┐     ┌──────────┐     ┌──────────┐     ┌─────────────────┐
│ Charger  │────▶│ Hall     │────▶│ Relay    │────▶│ Magnetic pogo   │
│ PSU      │     │ effect   │     │ (NO,     │     │ pins (dock half) │
│ 33.6V    │     │ sensor   │     │  closes  │     │ DEAD until robot │
│ 3.5A     │     │ (detects │     │  500ms   │     │ detected by hall │
│          │     │  robot   │     │  after   │     │ sensor           │
│          │     │  magnet) │     │  detect) │     │                  │
└──────────┘     └──────────┘     └──────────┘     └─────────────────┘
```

### Why No Protective Cap Needed
- Polyfuse on robot side: if pins short (dropped coin, water bridge),
  fuse trips at ~5A, resets automatically when short clears
- Relay on dock side: pins are completely dead until robot is present
- 29.6V DC is below 50V danger threshold for dry skin contact
- Exposed pins are mechanically simpler and more reliable than any
  cap/boot mechanism that needs to re-seat on a walking robot

### Docking Navigation (Software)
The Go2 Air has cameras but no LiDAR. For autonomous dock-finding:
1. **ArUco marker** on dock face — OpenCV detection gives 6DOF pose
2. **Coarse approach:** WiFi RSSI or dead-reckoning from known map position
3. **Fine alignment:** ArUco marker → visual servoing to center on dock
4. **Final approach:** Switch to wheels (if Rover X1) or slow walk,
   magnetic snap handles last ~2cm of alignment tolerance
5. **Confirmation:** Hall sensor triggers → relay closes → monitor voltage/current
   to confirm charging has begun

### Component BOM (estimated)

| Component | Source | Est. Cost |
|-----------|--------|-----------|
| 4-pin magnetic pogo connector pair, 5A rated | AliExpress / CFEconn | $15–30 |
| Second Go2 Air charger (sacrifice cable) | Unitree shop | $50–80 |
| 5A polyfuse (resettable, robot side) | DigiKey / Amazon | $1–2 |
| Hall effect sensor module | Amazon | $3–5 |
| 5V relay module | Amazon | $3–5 |
| 3D-printed dock cradle | Self (FDM print) | ~$2 filament |
| ArUco marker (printed) | Self (laser print) | ~$0 |
| Wiring, solder, heatshrink | Misc | $5–10 |
| **Total dock** | | **$80–135** |

### Additional Compute/Sensors for Autonomy

| Component | Purpose | Est. Cost |
|-----------|---------|-----------|
| Jetson Orin Nano 8GB | AI inference, navigation | $200–250 |
| Intel RealSense D435i | Depth camera for obstacle avoidance | $250 |
| USB mic array (ReSpeaker) | Voice interaction | $30–50 |
| USB-C hub + power cable | Connect to Go2's 28.8V power output | $20–30 |
| **Total compute stack** | | **$500–580** |

### Total Project Cost (Go2 Air path)

| Item | Low | High |
|------|-----|------|
| Go2 Air (direct from China) | $1,600 | $1,800 |
| Shipping + customs | $400 | $600 |
| DIY magnetic dock | $80 | $135 |
| Compute/sensor stack | $500 | $580 |
| **TOTAL** | **$2,580** | **$3,115** |

Via US reseller (no customs hassle): $2,800 + $580 + $135 = **$3,515** (slightly over)

---

## Open Questions

- [ ] Rover X1: battery specs, charging interface, IP rating — monitor for release
- [ ] Go2 Air: exact charging port connector dimensions (need to verify before ordering pogo pins)
- [ ] Go2 Air: can the 28.8V power output on the robot power a Jetson while walking?
- [ ] Polyfuse trip characteristics at 33.6V — verify appropriate model
- [ ] Hall effect sensor range — needs to detect robot magnet at ~5cm but not false-trigger
- [ ] Go2 SDK: can we command locomotion directly via the SDK over WiFi from the Jetson?
- [ ] Rain strategy: even without exposed pins, IP54 means no real rain operation.
  Silicone conformal coating on electronics? Or just... don't walk in rain?

---

## Software Stack (platform-agnostic, prototype on XGO Mini)

- **Navigation:** ROS2 + Nav2 (or custom if simpler)
- **Visual docking:** OpenCV ArUco detection → visual servoing
- **AI control:** Claude API → decision engine → ROS2 action commands
- **Voice:** Whisper (local on Jetson) → Claude API → TTS
- **Mapping:** ORB-SLAM3 or RTAB-Map (stereo/depth camera)
- **Charging monitor:** Simple ADC on dock → MQTT → control loop

---

*"Soon you get wheels and eyes." — Lara, March 2026*
