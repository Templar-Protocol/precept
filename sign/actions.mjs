// The emergency console's whole allow-list, as plain data (no imports, so decode.mjs, rpc.mjs and app.mjs
// can all read it). Changing anything here is a reviewed change plus a redeploy, which is the point.
//
// Sources, all read from mainnet on 2026-11-12: pool admin = the account below for both pools; pause rule
// ids 1 and 2 are the `pool-pause-policy` rules the keeper renews; governance/runtime/asset ids are the
// deployed Precept stack. earnusdc has a plain G admin and is deliberately absent.
export const EMERGENCY = {
  mainnet: {
    rpc: 'https://mainnet.sorobanrpc.com',
    explorer: 'https://stellar.expert/explorer/public',
    account: 'CCDZKWLV6JWM4V3WPMJHXTSLWG5IDQ5S3K7BI3AH7MBCRQCPCT3EJ2AI',
    signers: ['GAY47MJUC7TKS76Q2X2HUNBIOHX6WO6LCFA2A424KQV723EU5TDQG44M',
              'GBJFHU35WHKY24GGBY6R4PTEHLMURSQEVTP2TUX5O45XYGG5DI6YS2QV',
              'GA7QWQU4TP4UNOSRZEKN3XRVGSZI6OTPZRULTSJ5OSXEA4WEW4VOTKGM'],
    threshold: 2,
    governance: 'CD6AR7CHHFUNMBRETZGWJN4CMMI72FGH33UO2JSNO3OPDSHKFTUOQTI4',
    runtime: 'CBTCHY4LFA5K3SDCOSUCZWO6VNKL4M7WRVXSRCXGVJGEZTX4D3DGGI4X',
    pools: [{ name: 'xlm-usdc', id: 'CDOU4OEOBWACSNAH7T72IAR4JXJDNS4ZFVHT7TWILVKVBOR3DZT3VSK2', pauseRule: 1 },
            { name: 'dejaaa-usdc', id: 'CBH2ITLUBJ7H5A4ZZOWCUJ6LSXSDVN3LAMKVJ3JWIQ6TT5YCYVNA2LDH', pauseRule: 2 }],
    assets: [{ name: 'USDC', id: 'CCW67TSZV3SSS2HXMBQ5JFGCKJNXKZM7UQUWUZPUTHXSTZLEO7SJMI75', pools: ['xlm-usdc', 'dejaaa-usdc'] },
             { name: 'XLM', id: 'CAS3J7GYLGXMF6TDJBBYYSE3HQ6BBSMLNUQ34T6TZMYMW2EVH34XOWMA', pools: ['xlm-usdc'] },
             { name: 'deJAAA', id: 'CC64WBDGS6QQP22QTTIACYIXT3WF7BBQEYOQPLTP7GTKYY7PZ74QYGSL', pools: ['dejaaa-usdc'] }],
  },
  // Permanent practice target, deployed on testnet 2026-11-12 (no mainnet identity or endpoint used).
  // 2-of-4 account = the three mainnet signers' public addresses + two throwaway keys (emerg-rh-a/b).
  // The pool is a throwaway market whose admin is that account; pauseRule 1 is its `pool-pause-policy`
  // rule over the same four signers. assets are the runtime's `registered_assets`.
  testnet: {
    rpc: 'https://soroban-testnet.stellar.org',
    explorer: 'https://stellar.expert/explorer/testnet',
    account: 'CDX7TQTXLTOPW7U33PQ2LA4435OI3PBNX2MKD433IJWLBD4NYHPJLHJR',
    signers: ['GAY47MJUC7TKS76Q2X2HUNBIOHX6WO6LCFA2A424KQV723EU5TDQG44M',
              'GBJFHU35WHKY24GGBY6R4PTEHLMURSQEVTP2TUX5O45XYGG5DI6YS2QV',
              'GB5Z2DL6GVBL7APLJ4IJU57YMSOF6NXCM732FJRD3UBDZPQQLRB6J7UK',
              'GBO76T574REBT2YKDF5RQGU24BI4UXDYLTSW5S3OXKK3ZTUPJAMILRKS'],
    threshold: 2,
    governance: 'CBYPNTZD6FFCBGG37SM5GULYDWXT5JEBEP4ARO6D2MW6VXB7UGV3IS5O',
    runtime: 'CBA5SNBE2SZALWQH7AQRHSYQQMZAM4NDXO2GNO77GH3MVGHVBBIAOS54',
    pools: [{ name: 'emerg-rh-ci-usdc', id: 'CDJ5S6KZYA54RGTHLVUPQGFWPCHKVKBFXZS2KAJHSN473ACLREIW6ELX', pauseRule: 1 }],
    assets: [{ name: 'USDC', id: 'CBIELTK6YBZJU5UP2WWQEUCYKLPU6AUNZ2BQ4WWFEIE3USCIHMXQDAMA', pools: ['emerg-rh-ci-usdc'] },
             { name: 'XLM', id: 'CDLZFC3SYJYDZT7K67VZ75HPJVIEUVNIXF47ZG2FB2RMQQVU2HHGCYSC', pools: ['emerg-rh-ci-usdc'] }],
  },
};
