from jamjet.decorators import task


def test_bare_task_marks_method():
    @task
    async def m(self):
        return None

    assert hasattr(m, "__jamjet_task__")
    meta = m.__jamjet_task__
    assert meta["is_step"] is True
    assert meta["is_entrypoint"] is False


def test_task_entry_param():
    @task(entry=True)
    async def m(self):
        return None

    assert m.__jamjet_task__["is_entrypoint"] is True


def test_task_retry_timeout_params():
    @task(retry=3, timeout_s=30)
    async def m(self):
        return None

    assert m.__jamjet_task__["retry"] == 3
    assert m.__jamjet_task__["timeout_s"] == 30


def test_task_preserves_callable():
    @task
    async def m(self, x):
        return x * 2

    import asyncio
    assert asyncio.run(m(None, 21)) == 42
