# Ollama Models Supporting Tool Use

This document outlines Ollama models that natively support tool use, based on testing and external research.

## Models Confirmed to Support Tool Use

*   **`llama3:8b`**: Supports tool use.
*   **`llama3:70b`**: Supports tool use.

## Models Not Supporting Tool Use (as of current research)

*   **`mistral`**: Does not natively support tool use.
*   **`codellama`**: Does not natively support tool use.

## How to Test Tool Use

To test if a model supports tool use, you can try providing a tool definition and a prompt that would naturally invoke the tool. Observe if the model generates a `tool_code` block in its response.

Example (using `llama3`):

```python
from ollama import Client

client = Client(host='http://localhost:11434')

response = client.chat(
    model='llama3',
    messages=[
        {
            'role': 'user',
            'content': 'What is the weather like in Paris?',
        }
    ],
    tools=[
        {
            'type': 'function',
            'function': {
                'name': 'get_current_weather',
                'description': 'Get the current weather in a given location',
                'parameters': {
                    'type': 'object',
                    'properties': {
                        'location': {
                            'type': 'string',
                            'description': 'The city and state, e.g. San Francisco, CA',
                        },
                    },
                    'required': ['location'],
                },
            },
        }
    ],
)

print(response['message'])
```

If the model supports tool use, the response might look something like this:

```json
{
  "role": "assistant",
  "content": "",
  "tool_calls": [
    {
      "function": {
        "name": "get_current_weather",
        "arguments": {
          "location": "Paris, France"
        }
      }
    }
  ]
}
```

