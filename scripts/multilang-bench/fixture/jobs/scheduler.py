"""Scheduler: run background jobs periodically on a fixed interval."""
import time


class Scheduler:
    """Schedules callables to run every ``interval`` seconds."""

    def __init__(self, interval):
        self.interval = interval
        self.jobs = []

    def every(self, job):
        """Register a periodic background job."""
        self.jobs.append(job)

    def run_forever(self):
        """Run each scheduled job repeatedly, sleeping between ticks."""
        while True:
            for job in self.jobs:
                job()
            time.sleep(self.interval)
