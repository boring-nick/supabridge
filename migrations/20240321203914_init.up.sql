CREATE TABLE user_link (
    source_platform TEXT NOT NULL,
    source_user_id TEXT NOT NULL,
    target_platform TEXT NOT NULL,
    target_user_id TEXT NOT NULL,
    PRIMARY KEY(source_platform, source_user_id)
);
