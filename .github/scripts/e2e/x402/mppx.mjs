// The paying half of the mppx leg. mppx reaches the x402 rail through its own
// x402 protocol adapter, which reads the PAYMENT-REQUIRED header, synthesizes an
// evm/charge challenge from it, and answers in PAYMENT-SIGNATURE. Writes the
// response body where the shell leg asserts on it.
import { writeFileSync } from 'node:fs';
import { evm, Mppx } from 'mppx/client';
import { Header } from 'mppx/x402';
import { privateKeyToAccount } from 'viem/accounts';

const account = privateKeyToAccount(process.env.PAYER_KEY);
const client = Mppx.create({
  methods: [
    // Base Sepolia alone, so the mainnet offer carried in the same 402 is never
    // chosen, whatever order the server lists the offers in.
    evm.charge({
      account,
      currencies: [evm.assets.baseSepolia.USDC],
      networks: [evm.assets.toChainId('eip155:84532')],
    }),
  ],
  // The same 402 also advertises the MPP tempo challenge, which this method
  // cannot pay. Preferring the evm one keeps mppx from selecting tempo and
  // failing with UNSUPPORTED_METHOD.
  orderChallenges: (candidates) =>
    [...candidates].sort(
      (a, b) =>
        Number(b.challenge.method === 'evm') - Number(a.challenge.method === 'evm'),
    ),
  polyfill: false,
});

const response = await client.fetch(process.env.URL);
writeFileSync('response.txt', await response.text());
console.log(`status: ${response.status}`);

// Like @x402/fetch and unlike purl, this client surfaces the receipt header.
const receipt = response.headers.get('payment-response');
if (receipt) {
  console.log(`receipt: ${JSON.stringify(Header.decodePaymentResponse(receipt))}`);
}

if (!response.ok) {
  process.exit(1);
}
