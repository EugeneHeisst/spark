"""Generate Spark's original short UI cues (no third-party recordings)."""
from pathlib import Path
import math
import struct
import wave

RATE = 24000
# Each cue has its own melody/rhythm, with soft attack and release.
CUES = {
    'peer-join': [(659, .06), (988, .12)],
    'peer-leave': [(659, .07), (392, .13)],
    'connect': [(440, .09), (660, .09), (880, .16)],
    'disconnect': [(660, .09), (440, .10), (294, .16)],
    'notification': [(784, .08), (1047, .16)],
    'invite': [(523, .12), (659, .12), (784, .18), (0, .08), (659, .15)],
    'mic-on': [(587, .07), (880, .13)],
    'mic-off': [(440, .07), (294, .13)],
    'sound-on': [(392, .06), (587, .06), (784, .12)],
    'sound-off': [(784, .06), (587, .06), (392, .12)],
}
output = Path(__file__).resolve().parent.parent / 'assets' / 'sounds'
output.mkdir(parents=True, exist_ok=True)
for name, notes in CUES.items():
    samples = []
    for frequency, duration in notes:
        count = int(RATE * duration)
        for i in range(count):
            t = i / RATE
            envelope = min(1, t / .012) * min(1, (duration - t) / .045)
            tone = math.sin(2 * math.pi * frequency * t) + .15 * math.sin(4 * math.pi * frequency * t)
            samples.append(int(10000 * envelope * tone) if frequency else 0)
        samples.extend([0] * int(RATE * .018))
    with wave.open(str(output / f'{name}.wav'), 'wb') as f:
        f.setnchannels(1)
        f.setsampwidth(2)
        f.setframerate(RATE)
        f.writeframes(struct.pack(f'<{len(samples)}h', *samples))
