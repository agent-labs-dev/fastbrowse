from parallax.task import source_tasks

from fastbrowse_evals.task_specs import records
from fastbrowse_evals.versions import fingerprint, load_lock, relocked


def test_every_lock_fingerprint_is_the_parallax_hash_of_its_record() -> None:
    lock = load_lock()
    tasks = {task.id: task for task in source_tasks(source_names=("fastbrowse",))}
    assert lock == relocked(records())
    for task_id, entry in lock.items():
        assert tasks[task_id].hash.startswith(entry["fingerprint"])
        assert tasks[task_id].meta.version == entry["version"]


def test_a_changed_record_changes_its_fingerprint() -> None:
    record = records()[0]
    assert fingerprint(record) != fingerprint(record.model_copy(update={"prompt": record.prompt + " Be quick."}))
    assert fingerprint(record) != fingerprint(record.model_copy(update={"version": record.version + 1}))
