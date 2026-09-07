-- Brand colours derived from the organisation's logo.
--
-- Cached on the row rather than computed per request: extracting a dominant
-- colour means fetching and decoding an image, which has no business happening
-- while a customer waits for their loyalty card to render.
--
-- `brand_logo_source` records WHICH logo the colours came from, so a changed
-- logo re-derives and an unchanged one never does. Without it the only options
-- are recomputing forever or going stale forever.
ALTER TABLE organizations
    ADD COLUMN IF NOT EXISTS brand_background   text,
    ADD COLUMN IF NOT EXISTS brand_foreground   text,
    ADD COLUMN IF NOT EXISTS brand_accent       text,
    ADD COLUMN IF NOT EXISTS brand_logo_source  text;

-- Derived values only ever hold `#RRGGBB`; anything else would reach a customer's
-- card as a broken style.
ALTER TABLE organizations
    ADD CONSTRAINT organizations_brand_hex CHECK (
        (brand_background IS NULL OR brand_background ~ '^#[0-9A-Fa-f]{6}$') AND
        (brand_foreground IS NULL OR brand_foreground ~ '^#[0-9A-Fa-f]{6}$') AND
        (brand_accent     IS NULL OR brand_accent     ~ '^#[0-9A-Fa-f]{6}$')
    );
