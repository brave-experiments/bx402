// The paying half of the mppx leg. mppx reaches the x402 rail through its own
// x402 protocol adapter, reading PAYMENT-REQUIRED and answering in
// PAYMENT-SIGNATURE.
import { writeFileSync } from 'node:fs';
import { evm, Mppx } from 'mppx/client';
import { Header } from 'mppx/x402';
import { privateKeyToAccount } from 'viem/accounts';

const account = privateKeyToAccount(process.env.PAYER_KEY);
const client = Mppx.create({
  methods: [
    // Base Sepolia alone, so the mainnet offer in the same 402 is never paid.
    evm.charge({
      account,
      currencies: [evm.assets.baseSepolia.USDC],
      networks: [evm.assets.toChainId('eip155:84532')],
    }),
  ],
  polyfill: false,
});

const response = await client.fetch(process.env.URL);
writeFileSync('response.txt', await response.text());
console.log(`status: ${response.status}`);

const receipt = response.headers.get('payment-response');
if (receipt) {
  console.log(`receipt: ${JSON.stringify(Header.decodePaymentResponse(receipt))}`);
}

if (!response.ok) {
  process.exit(1);
}
