-- lua-chain.lua
-- Lua leaf module used by fnl-chain.fnl to test the nested require scenario:
--   main.fnl  →  fnl-chain.fnl  →  lua-chain.lua

local function double(x)
  return x * 2
end

local function square(x)
  return x * x
end
