-- Persist the currency of average_cost so /networth can FX-convert book cost
-- without inferring from live Yahoo quote currencies.
ALTER TABLE portfolio_holdings ADD COLUMN average_cost_currency TEXT;
