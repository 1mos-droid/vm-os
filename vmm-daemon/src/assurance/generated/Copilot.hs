import           Copilot.Compile.C99
import           Copilot.Language              hiding (prop)
import           Copilot.Language.Prelude
import           Copilot.Library.LTL           (next)
import           Copilot.Library.MTL           hiding (since, alwaysBeen, trigger)
import           Copilot.Library.PTLTL         (since, previous, alwaysBeen)
import qualified Copilot.Library.PTLTL         as PTLTL
import qualified Copilot.Library.MTL           as MTL
import           Copilot.Library.StateMachines (stateMachine)
import           Language.Copilot              (reify)
import           Prelude                       hiding ((&&), (||), (++), (<=), (>=), (<), (>), (==), (/=), not)

nat_spaces_running :: Stream (Int64)
nat_spaces_running = extern "nat_spaces_running" Nothing

nat_spaces_lan_blocked :: Stream (Int64)
nat_spaces_lan_blocked = extern "nat_spaces_lan_blocked" Nothing

reserved_ram_mib :: Stream (Int64)
reserved_ram_mib = extern "reserved_ram_mib" Nothing

ram_budget_mib :: Stream (Int64)
ram_budget_mib = extern "ram_budget_mib" Nothing

largest_capture_bytes :: Stream (Int64)
largest_capture_bytes = extern "largest_capture_bytes" Nothing

-- | HYP001
--   @
--   host shall always satisfy nat_spaces_lan_blocked = nat_spaces_running
--   @
propHYP001 :: Stream Bool
propHYP001 = (PTLTL.alwaysBeen (nat_spaces_lan_blocked == nat_spaces_running))

-- | HYP002
--   @
--   host shall always satisfy reserved_ram_mib <= ram_budget_mib
--   @
propHYP002 :: Stream Bool
propHYP002 = (PTLTL.alwaysBeen (reserved_ram_mib <= ram_budget_mib))

-- | HYP003
--   @
--   host shall always satisfy largest_capture_bytes <= 268435456
--   @
propHYP003 :: Stream Bool
propHYP003 = (PTLTL.alwaysBeen (largest_capture_bytes <= 268435456))

-- | Clock that increases in one-unit steps.
clock :: Stream Int64
clock = [0] ++ (clock + 1)

-- | First Time Point
ftp :: Stream Bool
ftp = [True] ++ false

pre :: Stream Bool -> Stream Bool
pre = ([False] ++)

tpre :: Stream Bool -> Stream Bool
tpre = ([True] ++)

notPreviousNot :: Stream Bool -> Stream Bool
notPreviousNot = not . PTLTL.previous . not

-- | Complete specification. Calls C handler functions when properties are
-- violated.
spec :: Spec
spec = do
  trigger "handlerHYP001" (not propHYP001) []
  trigger "handlerHYP002" (not propHYP002) []
  trigger "handlerHYP003" (not propHYP003) []

main :: IO ()
main = reify spec >>= compile "monitor"
