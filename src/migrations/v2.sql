-- Board cards need checklist progress without one detail request per card.
-- Without this index the per-project aggregate scans the whole checklist table.
CREATE INDEX checklist_task ON checklist (task_id);
