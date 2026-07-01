"""A simple first-in first-out (FIFO) in-memory task queue."""


class TaskQueue:
    """A FIFO queue of pending tasks."""

    def __init__(self):
        self._items = []

    def enqueue(self, task):
        """Append a task to the back of the queue."""
        self._items.append(task)

    def dequeue(self):
        """Pop the oldest task from the front of the queue."""
        return self._items.pop(0) if self._items else None
