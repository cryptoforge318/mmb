CREATE TABLE balance_updates (
    id bigint PRIMARY KEY GENERATED BY DEFAULT AS IDENTITY,
    insert_time timestamp WITH TIME ZONE NOT NULL DEFAULT now(),
    version int,
    json jsonb NOT NULL
);

CREATE INDEX balance_updates__insert_time_idx ON balance_updates USING btree (insert_time);
