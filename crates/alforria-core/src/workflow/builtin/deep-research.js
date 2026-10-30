export const meta = {
  name: 'deep-research',
  description: 'Research a question across several angles, cross-check the claims, and return a cited report.',
  phases: ['Plan', 'Research', 'Verify', 'Report'],
}

const question = typeof args === 'string' ? args : (args && (args.question || args.query)) || ''
if (!question) throw new Error('deep-research needs a question: pass args as a string or {"question": "..."}')

phase('Plan')
const plan = await agent(
  `Break this research question into 3 to 5 distinct angles that together cover it:\n\n${question}\n\nReturn ONLY JSON: {"angles": [{"title": "...", "brief": "..."}]}`,
  {
    label: 'plan',
    schema: {
      type: 'object',
      required: ['angles'],
      properties: {
        angles: {
          type: 'array',
          items: {
            type: 'object',
            required: ['title', 'brief'],
            properties: { title: { type: 'string' }, brief: { type: 'string' } },
          },
        },
      },
    },
  },
)
const angles = Array.isArray(plan) ? plan : plan && Array.isArray(plan.angles) ? plan.angles : []
if (!angles.length) throw new Error('the planning agent returned no research angles')

phase('Research')
const findings = await pipeline(angles, (angle) =>
  agent(
    `Research this angle of the question "${question}".\n\nAngle: ${angle.title}\nFocus: ${angle.brief}\n\nUse web search and fetch pages. Report concrete findings and cite every source URL. Flag anything you could not confirm.`,
    { label: angle.title },
  ),
)

phase('Verify')
const verified = await parallel(
  findings.map((finding, index) =>
    finding
      ? agent(
          `Adversarially check the claims in this research note. Keep only claims supported by a named source; mark the rest unverified. Return a corrected note.\n\n${finding}`,
          { label: `verify ${index + 1}` },
        )
      : null,
  ),
)

const notes = verified.filter(Boolean)
if (!notes.length) throw new Error('every research agent failed, so there is nothing to report')

phase('Report')
const report = await agent(
  `Write one cited report answering: ${question}\n\nUse only these verified research notes. Merge duplicates, rank findings by importance, and keep source URLs next to the claims they support. Put unverified claims in a short final section.\n\n${notes.join('\n\n---\n\n')}`,
  { label: 'report' },
)

return report
