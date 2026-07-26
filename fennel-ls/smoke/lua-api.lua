-- lua-api.lua
-- A plain Lua module loaded by lua-consumer.fnl to test cross-language require.
-- Functions are defined as top-level locals so they appear in the analysis as
-- direct top-level defs (no module table needed for static analysis).

local function add(a, b)
  return a + b
end

local function greet(name)
  return "Hello, " .. name
end

local function answer()
  return 42
end
