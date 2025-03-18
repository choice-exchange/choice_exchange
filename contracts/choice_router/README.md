# Choice Router <!-- omit in toc -->

The Router Contract contains the logic to facilitate multi-hop swap operations via choice exchange.

## Operations Assertion

The contract will check whether the resulting token is swapped into one token.

### Example

Swap INJ  =>  CW20_TOKEN  =>  CW20_TOKEN_2

```json
{
   "execute_swap_operations":{
      "operations":[
         {
            "choice":{
               "offer_asset_info":{
                  "native_token":{
                     "denom":"inj"
                  }
               },
               "ask_asset_info":{
                  "token":{
                     "contract_addr":"injcw20contract..."
                  }
               }
            }
         },
         {
            "choice":{
               "offer_asset_info":{
                  "token":{
                     "contract_addr":"injcw20contract..."
                  }
               },
               "ask_asset_info":{
                  "token":{
                     "contract_addr":"injcw20contract2..."
                  }
               }
            }
         }
      ],
      "minimum_receive":"1"
   }
}
```
