"""Optional scheduler, only started with `--profile scheduler`."""
import time

while True:
    print("tick", flush=True)
    time.sleep(10)
