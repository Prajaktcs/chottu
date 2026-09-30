-- Persist successful private food flags independently of food mutations.
-- Undo/clear must not cause the same tag to nag a member twice in one day.
CREATE TABLE IF NOT EXISTS condition_food_flags (
    family_member_id TEXT NOT NULL,
    date TEXT NOT NULL,
    tag TEXT NOT NULL,
    PRIMARY KEY (family_member_id, date, tag)
);
