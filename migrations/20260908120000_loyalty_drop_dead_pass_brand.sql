-- The pass's brand comes from the ORGANISATION, not from the loyalty programme.
--
-- These four were the programme's own colour/logo overrides, written by a
-- settings form that no longer exists: branding moved to `organizations`
-- (name + logo_url + the palette derived from that logo) so a shop configures
-- its identity once and every surface paints the same card.
--
-- They are dropped rather than left in place because they were still being
-- READ. The Apple pass took its colours from here, found the NULLs a dead form
-- had left behind, and shipped every pass in Apple's default grey while the web
-- card was correctly themed. A column nothing writes but something reads is not
-- dead weight, it is a trap.
ALTER TABLE loyalty_settings
    DROP COLUMN IF EXISTS pass_background_color,
    DROP COLUMN IF EXISTS pass_foreground_color,
    DROP COLUMN IF EXISTS pass_label_color,
    DROP COLUMN IF EXISTS pass_logo_url;
