-- Search (decision 12): PostgreSQL full-text search with the `simple` configuration plus
-- unaccent. `simple` neither stems nor drops stop words, so every language tokenizes alike;
-- it does not segment CJK text, which is the known v1 limit.
CREATE EXTENSION IF NOT EXISTS unaccent;
CREATE TEXT SEARCH CONFIGURATION commons_search (COPY = simple);
ALTER TEXT SEARCH CONFIGURATION commons_search
    ALTER MAPPING FOR word, hword, hword_part, numword, numhword WITH unaccent, simple;

ALTER TABLE messages ADD COLUMN search tsvector
    GENERATED ALWAYS AS (to_tsvector('commons_search', body)) STORED;
CREATE INDEX messages_search ON messages USING gin (search);

-- File names are indexed whole and split at separators, so "q3-report.pdf" matches "report".
ALTER TABLE files
    ADD COLUMN name_search tsvector GENERATED ALWAYS AS (
        to_tsvector('commons_search', name || ' ' || regexp_replace(name, '[._/\\-]+', ' ', 'g'))
    ) STORED,
    ADD COLUMN text_search tsvector GENERATED ALWAYS AS (
        to_tsvector('commons_search', coalesce(extracted_text, ''))
    ) STORED;
CREATE INDEX files_name_search ON files USING gin (name_search);
CREATE INDEX files_text_search ON files USING gin (text_search);
